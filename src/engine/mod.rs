mod order;

use crate::common::config::EngineConfig;
use crate::strategy::Strategy;
use crate::strategy::{BonoStrategy, KonzervaStrategy};
use crate::types::{Market, TickContext, TickResult, Trade};
use alloy_signer_local::PrivateKeySigner;
use log::{info, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::Client as ClobClient;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

fn create_strategy(name: &str) -> Box<dyn Strategy> {
    match name {
        "bono" => Box::new(BonoStrategy::new()),
        "konzerva" => Box::new(KonzervaStrategy::new()),
        _ => panic!("Unknown strategy: '{}'. Supported: bono, konzerva", name),
    }
}

pub struct StrategyEngine {
    strategy: Box<dyn Strategy>,
    pub paper_mode: bool,
    pub config: EngineConfig,

    // Auth / SDK
    client: Option<ClobClient<Authenticated<Normal>>>,
    signer_instance: Option<PrivateKeySigner>,

    // Trade history for current market
    trades: Vec<Trade>,

    // Loop counters
    last_log_ts: i64,

    // Last try_order invocation timestamp (ms)
    pub last_try_order_failed_timestamp_ms: i64,

    // Watch Receivers
    pub binance_rx: watch::Receiver<(f64, i64)>,
    pub coinbase_rx: watch::Receiver<(f64, i64)>,
    pub chainlink_rx: watch::Receiver<(f64, i64)>,
    pub dvol_rx: watch::Receiver<(f64, i64)>,
    pub shared_market: Arc<Mutex<Option<Market>>>,
    pub binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    pub chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    pub dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>>,

    // Budget (USDC), shared — safe to read/write from anywhere
    pub budget: Arc<Mutex<f64>>,
}

impl StrategyEngine {
    pub fn new(
        paper_mode: bool,
        config: EngineConfig,
        binance_rx: watch::Receiver<(f64, i64)>,
        coinbase_rx: watch::Receiver<(f64, i64)>,
        chainlink_rx: watch::Receiver<(f64, i64)>,
        dvol_rx: watch::Receiver<(f64, i64)>,
        shared_market: Arc<Mutex<Option<Market>>>,
        binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        budget: Arc<Mutex<f64>>,
    ) -> Self {
        let strategy = create_strategy(&config.strategy);
        Self {
            strategy,
            paper_mode,
            config,
            client: None,
            signer_instance: None,
            trades: Vec::new(),
            last_log_ts: 0,
            last_try_order_failed_timestamp_ms: 0,
            binance_rx,
            coinbase_rx,
            chainlink_rx,
            dvol_rx,
            shared_market,
            binance_history,
            chainlink_history,
            dvol_history,
            budget,
        }
    }

    pub fn set_client(
        &mut self,
        client: ClobClient<Authenticated<Normal>>,
        signer: PrivateKeySigner,
    ) {
        self.client = Some(client);
        self.signer_instance = Some(signer);
    }

    /// Resets per-market state. Call between market rotations.
    pub fn clear_state(&mut self) {
        self.trades.clear();
        self.last_log_ts = 0;
        self.last_try_order_failed_timestamp_ms = 0;
    }

    /// Runs one engine tick. Returns a TickResult with any trades filled.
    pub async fn execute_tick(&mut self) -> TickResult {
        let ctx = self.snapshot();
        let timestamp_ms = ctx.polymarket_now_ms;

        let traded = if let Some(t) = self.try_order(&ctx).await {
            let cost = t.intent.cost();
            if cost > 0.0 {
                let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
                *budget -= cost;
                info!("Trade (Buy) {:?} | budget={:.2}", t, *budget);
            } else {
                info!("Trade (Sell) {:?}", t);
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

    fn snapshot(&mut self) -> TickContext {
        let (binance_price, binance_ts) = *self.binance_rx.borrow();
        let (coinbase_price, coinbase_ts) = *self.coinbase_rx.borrow();
        let (chainlink_price, chainlink_ts) = *self.chainlink_rx.borrow();
        let (dvol, dvol_ts) = *self.dvol_rx.borrow();
        let polymarket_now_ms = crate::common::time::now_ms();
        let market = self
            .shared_market
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let binance_reliable =
            self.check_binance_reliable(binance_price, coinbase_price, polymarket_now_ms);

        TickContext {
            binance_price,
            binance_ts,
            coinbase_price,
            coinbase_ts,
            chainlink_price,
            chainlink_ts,
            dvol,
            dvol_ts,
            polymarket_now_ms,
            binance_reliable,
            market,
            binance_history: self.binance_history.clone(),
            chainlink_history: self.chainlink_history.clone(),
            dvol_history: self.dvol_history.clone(),
            trades: self.trades.clone(),
        }
    }

    fn check_binance_reliable(
        &mut self,
        binance_price: f64,
        coinbase_price: f64,
        now_ms: i64,
    ) -> bool {
        let divergence = (binance_price - coinbase_price).abs();
        if divergence > self.config.exchange_price_divergence_threshold
            && coinbase_price > 0.0
            && binance_price > 0.0
        {
            let log_interval_ms = (self.config.log_interval_secs * 1000.0) as i64;
            if now_ms - self.last_log_ts >= log_interval_ms {
                self.last_log_ts = now_ms;
                warn!(
                    "Binance unreliable! Binance=${:.2} Coinbase=${:.2} divergence=${:.2}",
                    binance_price, coinbase_price, divergence
                );
            }
            return false;
        }
        true
    }
}
