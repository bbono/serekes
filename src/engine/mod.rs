mod order;
mod run;

use crate::integrations::lookup_history;
use crate::strategy::Strategy;
use crate::strategy::{BonoStrategy, KonzervaStrategy};
use crate::types::{Market, TickContext, TickResult, Trade};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{debug, error, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as _};
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::POLYGON;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
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
    paper_mode: bool,

    // Auth / SDK
    client: Option<ClobClient<Authenticated<Normal>>>,
    signer_instance: Option<PrivateKeySigner>,

    // Trade history for current market
    trades: Vec<Trade>,

    // Last try_order invocation timestamp (ms)
    last_try_order_failed_timestamp_ms: i64,

    // Watch Receivers
    binance_rx: watch::Receiver<(f64, i64)>,
    coinbase_rx: watch::Receiver<(f64, i64)>,
    chainlink_rx: watch::Receiver<(f64, i64)>,
    shared_market: Arc<Mutex<Option<Arc<Market>>>>,
    binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,

    // Budget (USDC), shared — safe to read/write from anywhere
    budget: Arc<Mutex<f64>>,
}

impl StrategyEngine {
    pub fn new(
        paper_mode: bool,
        strategy_name: &str,
        binance_rx: watch::Receiver<(f64, i64)>,
        coinbase_rx: watch::Receiver<(f64, i64)>,
        chainlink_rx: watch::Receiver<(f64, i64)>,
        shared_market: Arc<Mutex<Option<Arc<Market>>>>,
        binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        budget: Arc<Mutex<f64>>,
    ) -> Self {
        let strategy = create_strategy(strategy_name);
        Self {
            strategy,
            paper_mode,
            client: None,
            signer_instance: None,
            trades: Vec::new(),
            last_try_order_failed_timestamp_ms: 0,
            binance_rx,
            coinbase_rx,
            chainlink_rx,
            shared_market,
            binance_history,
            chainlink_history,
            budget,
        }
    }

    /// Authenticate with Polymarket SDK using a private key.
    pub async fn authenticate(&mut self, private_key: &str) {
        let signer: PrivateKeySigner = LocalSigner::from_str(private_key)
            .unwrap()
            .with_chain_id(Some(POLYGON));
        let client_builder =
            ClobClient::new("https://clob.polymarket.com", ClobConfig::builder().build()).unwrap();

        match client_builder
            .authentication_builder(&signer)
            .signature_type(SignatureType::Proxy)
            .authenticate()
            .await
        {
            Ok(client) => {
                self.client = Some(client);
                self.signer_instance = Some(signer);
                debug!("SDK authenticated");
            }
            Err(e) => {
                error!("SDK auth failed: {e}");
            }
        }
    }

    /// Resets per-market state. Call between market rotations.
    fn clear_state(&mut self) {
        self.trades.clear();
        self.last_try_order_failed_timestamp_ms = 0;
    }

    /// Runs one engine tick. Returns a TickResult with any trades filled.
    async fn execute_tick(&mut self) -> TickResult {
        let ctx = self.snapshot();
        let timestamp_ms = ctx.polymarket_now_ms;

        let traded = if let Some(t) = self.try_order(&ctx).await {
            let cost = t.intent.cost();
            if cost > 0.0 {
                let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
                *budget -= cost;
                debug!(
                    "Buy {:?} price={:.4} size={:.4} status={:?} budget={:.2}",
                    t.direction, t.price, t.size, t.order_status, *budget
                );
            } else {
                debug!(
                    "Sell {:?} price={:.4} size={:.4} status={:?} budget={:.2}",
                    t.direction, t.price, t.size, t.order_status,
                    *self.budget.lock().unwrap_or_else(|e| e.into_inner())
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
                chainlink_strike = lookup_history(&self.chainlink_history, market.started_at_ms, true);
            }
            if binance_strike == 0.0 {
                binance_strike = lookup_history(&self.binance_history, market.started_at_ms, false);
            }
            if chainlink_strike > 0.0 && binance_strike > 0.0 {
                return Some((chainlink_strike, binance_strike));
            }
            if crate::common::time::now_ms() > deadline_ms {
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
        let (binance_price, binance_ts) = *self.binance_rx.borrow();
        let (coinbase_price, coinbase_ts) = *self.coinbase_rx.borrow();
        let (chainlink_price, chainlink_ts) = *self.chainlink_rx.borrow();
        let polymarket_now_ms = crate::common::time::now_ms();
        let market = self
            .shared_market
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        TickContext {
            binance_price,
            binance_ts,
            coinbase_price,
            coinbase_ts,
            chainlink_price,
            chainlink_ts,
            polymarket_now_ms,
            market,
            binance_history: self.binance_history.clone(),
            chainlink_history: self.chainlink_history.clone(),
            trades: self.trades.clone(),
        }
    }
}
