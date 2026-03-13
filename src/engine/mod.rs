mod order;

use crate::common::config::EngineConfig;
use crate::strategy::{BonoStrategy, KonzervaStrategy};
use crate::strategy::Strategy;
use crate::types::{Market, TickContext, Trade};
use alloy_signer_local::PrivateKeySigner;
use log::warn;
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

    // Watch Receivers
    pub binance_rx: watch::Receiver<(f64, i64)>,
    pub coinbase_rx: watch::Receiver<(f64, i64)>,
    pub chainlink_rx: watch::Receiver<(f64, i64)>,
    pub dvol_rx: watch::Receiver<(f64, i64)>,
    pub shared_market: Arc<Mutex<Option<Market>>>,
    pub binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    pub chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    pub dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
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
            binance_rx,
            coinbase_rx,
            chainlink_rx,
            dvol_rx,
            shared_market,
            binance_history,
            chainlink_history,
            dvol_history,
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
    }

    /// Returns Some(Trade) when the strategy places an order.
    pub async fn execute_tick(&mut self) -> Option<Trade> {
        let ctx = self.snapshot();

        // 1. Killswitch — block new entries if exchanges diverge
        if self.check_killswitch(&ctx) {
            return None;
        }

        // 2. Try to place an order
        self.try_order(&ctx).await
    }

    fn snapshot(&self) -> TickContext {
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
            market,
            binance_history: self.binance_history.clone(),
            chainlink_history: self.chainlink_history.clone(),
            dvol_history: self.dvol_history.clone(),
            trades: self.trades.clone(),
        }
    }

    fn check_killswitch(&mut self, ctx: &TickContext) -> bool {
        let divergence = (ctx.binance_price - ctx.coinbase_price).abs();
        if divergence > self.config.exchange_price_divergence_threshold
            && ctx.coinbase_price > 0.0
            && ctx.binance_price > 0.0
        {
            let log_interval_ms = (self.config.log_interval_secs * 1000.0) as i64;
            if ctx.polymarket_now_ms - self.last_log_ts >= log_interval_ms {
                self.last_log_ts = ctx.polymarket_now_ms;
                warn!(
                    "Killswitch! Binance=${:.2} Coinbase=${:.2} divergence=${:.2}",
                    ctx.binance_price, ctx.coinbase_price, divergence
                );
            }
            return true;
        }
        false
    }
}
