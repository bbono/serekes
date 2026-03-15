mod order;
mod run;

use crate::domain::strategy::Strategy;
use crate::domain::strategy::{BonoStrategy, KonzervaStrategy};
use crate::domain::{Market, TickContext, TickResult, Trade};
use crate::ports::{ClockPort, ExchangePort, MarketDiscoveryPort, MarketPricePort, PersistencePort, PriceFeedPort};
use log::{info, warn};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

fn create_strategy(name: &str) -> Box<dyn Strategy> {
    match name {
        "bono" => Box::new(BonoStrategy::new()),
        "konzerva" => Box::new(KonzervaStrategy::new()),
        _ => panic!("Unknown strategy: '{}'. Supported: bono, konzerva", name),
    }
}

pub struct StrategyEngine {
    strategy: Box<dyn Strategy>,

    // --- Outbound ports ---
    binance_feed: Arc<dyn PriceFeedPort>,
    coinbase_feed: Arc<dyn PriceFeedPort>,
    chainlink_feed: Arc<dyn PriceFeedPort>,
    market_price: Arc<dyn MarketPricePort>,
    exchange: Arc<dyn ExchangePort>,
    discovery: Arc<dyn MarketDiscoveryPort>,
    persistence: Arc<dyn PersistencePort>,
    clock: Arc<dyn ClockPort>,

    // --- Per-market state ---
    trades: Vec<Trade>,

    // Budget (USDC), shared — safe to read/write from anywhere
    budget: Arc<Mutex<f64>>,
}

impl StrategyEngine {
    pub fn new(
        strategy_name: &str,
        binance_feed: Arc<dyn PriceFeedPort>,
        coinbase_feed: Arc<dyn PriceFeedPort>,
        chainlink_feed: Arc<dyn PriceFeedPort>,
        market_price: Arc<dyn MarketPricePort>,
        exchange: Arc<dyn ExchangePort>,
        discovery: Arc<dyn MarketDiscoveryPort>,
        persistence: Arc<dyn PersistencePort>,
        clock: Arc<dyn ClockPort>,
        budget: Arc<Mutex<f64>>,
    ) -> Self {
        let strategy = create_strategy(strategy_name);
        Self {
            strategy,
            binance_feed,
            coinbase_feed,
            chainlink_feed,
            market_price,
            exchange,
            discovery,
            persistence,
            clock,
            trades: Vec::new(),
            budget,
        }
    }

    /// Resets per-market state. Call between market rotations.
    fn clear_state(&mut self) {
        self.trades.clear();
    }

    /// Runs one engine tick. Returns a TickResult with any trades filled.
    async fn execute_tick(&mut self) -> TickResult {
        let ctx = self.snapshot();
        let timestamp_ms = ctx.polymarket_now_ms;

        let traded = if let Some(t) = self.try_order(&ctx).await {
            let cost = t.intent.cost();
            let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
            if cost > 0.0 {
                *budget -= cost;
                info!(
                    "Buy {:?} price={:.4} size={:.4} status={:?} budget={:.2}",
                    t.direction, t.price, t.size, t.order_status, *budget
                );
            } else {
                let proceeds = t.price * t.size;
                *budget += proceeds;
                info!(
                    "Sell {:?} price={:.4} size={:.4} status={:?} budget={:.2}",
                    t.direction, t.price, t.size, t.order_status, *budget
                );
            }
            true
        } else {
            false
        };

        let budget = *self.budget.lock().unwrap_or_else(|e| e.into_inner());
        TickResult {
            traded,
            trades: self.trades.clone(),
            timestamp_ms,
            completed: budget < 1.0,
        }
    }

    /// Resolve strike prices for a market from price histories.
    /// Chainlink uses exact timestamp match (settlement oracle);
    /// Binance uses latest price at or before market start.
    async fn resolve_strike_prices(&self, market: &Market) -> Option<(f64, f64)> {
        let deadline_ms = market.started_at_ms + 10_000;
        let mut chainlink_strike = 0.0f64;
        let mut binance_strike = 0.0f64;
        loop {
            if chainlink_strike == 0.0 {
                chainlink_strike = self.chainlink_feed.lookup_history(market.started_at_ms, true);
            }
            if binance_strike == 0.0 {
                binance_strike = self.binance_feed.lookup_history(market.started_at_ms, false);
            }
            if chainlink_strike > 0.0 && binance_strike > 0.0 {
                return Some((chainlink_strike, binance_strike));
            }
            if self.clock.now_ms() > deadline_ms {
                warn!(
                    "Strike resolution timed out for {}: chainlink={} binance={}",
                    market.slug,
                    if chainlink_strike > 0.0 { format!("{:.2}", chainlink_strike) } else { "missing".into() },
                    if binance_strike > 0.0 { format!("{:.2}", binance_strike) } else { "missing".into() },
                );
                return None;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    fn snapshot(&mut self) -> TickContext {
        let (binance_price, binance_ts) = self.binance_feed.latest();
        let (coinbase_price, coinbase_ts) = self.coinbase_feed.latest();
        let (chainlink_price, chainlink_ts) = self.chainlink_feed.latest();
        let polymarket_now_ms = self.clock.now_ms();
        let market = self.market_price.current_market();

        TickContext {
            binance_price,
            binance_ts,
            coinbase_price,
            coinbase_ts,
            chainlink_price,
            chainlink_ts,
            polymarket_now_ms,
            market,
            trades: self.trades.clone(),
        }
    }
}
