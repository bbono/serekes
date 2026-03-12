pub mod strategies;
pub mod traits;

pub use strategies::BonoStrategy;
pub use traits::Strategy;

use crate::config::EngineConfig;
use crate::types::{Market, MarketOrderType, OrderParams, Side, TickContext, TokenDirection, Trade};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{error, info, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as SDKSigner};
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side as SdkSide, SignatureType};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::POLYGON;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;


// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineState {
    Idle,
    InPosition,
}

pub struct StrategyEngine<S: Strategy> {
    pub strategy: S,
    pub paper_mode: bool,
    pub time_offset_ms: i64,
    pub engine_config: EngineConfig,

    // Auth / SDK
    client: Option<ClobClient<Authenticated<Normal>>>,
    signer_instance: Option<PrivateKeySigner>,

    // State machine
    state: EngineState,

    // Loop counters
    last_killswitch_log_ts: i64,
    last_engine_state_log_ts: i64,

    // Watch Receivers
    pub binance_rx: watch::Receiver<(f64, i64)>,
    pub coinbase_rx: watch::Receiver<(f64, i64)>,
    pub chainlink_rx: watch::Receiver<(f64, i64)>,
    pub dvol_rx: watch::Receiver<f64>,
    pub shared_market: Arc<Mutex<Option<Market>>>,
    pub binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    pub chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
}

// ---------------------------------------------------------------------------
// Constructor & auth
// ---------------------------------------------------------------------------

impl<S: Strategy> StrategyEngine<S> {
    pub fn new(
        strategy: S,
        paper_mode: bool,
        time_offset_ms: i64,
        engine_config: EngineConfig,
        binance_rx: watch::Receiver<(f64, i64)>,
        coinbase_rx: watch::Receiver<(f64, i64)>,
        chainlink_rx: watch::Receiver<(f64, i64)>,
        dvol_rx: watch::Receiver<f64>,
        shared_market: Arc<Mutex<Option<Market>>>,
        binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    ) -> Self {
        Self {
            strategy,
            paper_mode,
            time_offset_ms,
            engine_config,
            client: None,
            signer_instance: None,
            state: EngineState::Idle,
            last_killswitch_log_ts: 0,
            last_engine_state_log_ts: 0,
            binance_rx,
            coinbase_rx,
            chainlink_rx,
            dvol_rx,
            shared_market,
            binance_history,
            chainlink_history,
        }
    }

    pub async fn initialize_client(&mut self, private_key: &str) -> bool {
        let signer: PrivateKeySigner = LocalSigner::from_str(private_key)
            .unwrap()
            .with_chain_id(Some(POLYGON));
        let client_builder =
            ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();

        match client_builder
            .authentication_builder(&signer)
            .signature_type(SignatureType::Proxy)
            .authenticate()
            .await
        {
            Ok(client) => {
                self.client = Some(client);
                self.signer_instance = Some(signer);
                true
            }
            Err(e) => {
                error!("SDK auth failed: {}", e);
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tick — the main entry point
    // -----------------------------------------------------------------------

    /// Returns Some(Trade) when the strategy places an order.
    pub async fn execute_tick(&mut self) -> Option<Trade> {
        let ctx = self.snapshot();

        // 1. Killswitch — block new entries if exchanges diverge
        if self.check_killswitch(&ctx) {
            return None;
        }

        // 2. Try buy
        let trade = if self.state == EngineState::Idle {
            self.try_buy(&ctx).await
        } else {
            None
        };

        let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
        if ctx.now_ms - self.last_engine_state_log_ts >= log_interval_ms {
            self.last_engine_state_log_ts = ctx.now_ms;
            info!("[{:?}]", self.state);
        }

        trade
    }

    // -----------------------------------------------------------------------
    // Tick helpers
    // -----------------------------------------------------------------------

    fn snapshot(&self) -> TickContext {
        let (binance_price, binance_ts) = *self.binance_rx.borrow();
        let (coinbase_price, coinbase_ts) = *self.coinbase_rx.borrow();
        let (chainlink_price, chainlink_ts) = *self.chainlink_rx.borrow();
        let dvol = *self.dvol_rx.borrow();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + self.time_offset_ms;
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
            now_ms,
            market,
            binance_history: self.binance_history.clone(),
            chainlink_history: self.chainlink_history.clone(),
        }
    }

    fn check_killswitch(&mut self, ctx: &TickContext) -> bool {
        let divergence = (ctx.binance_price - ctx.coinbase_price).abs();
        if divergence > self.engine_config.exchange_price_divergence_threshold
            && ctx.coinbase_price > 0.0
            && ctx.binance_price > 0.0
        {
            let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
            if ctx.now_ms - self.last_killswitch_log_ts >= log_interval_ms {
                self.last_killswitch_log_ts = ctx.now_ms;
                warn!(
                    "Killswitch! Binance=${:.2} Coinbase=${:.2} divergence=${:.2}",
                    ctx.binance_price, ctx.coinbase_price, divergence
                );
            }
            return true;
        }
        false
    }

    async fn try_buy(
        &mut self,
        ctx: &TickContext,
    ) -> Option<Trade> {
        let market = ctx.market.as_ref()?;
        let (direction, order) = self.strategy.create_entry_order(ctx)?;
        let token_id = match direction {
            TokenDirection::Up => market.up.token_id.clone(),
            TokenDirection::Down => market.down.token_id.clone(),
        };

        // --- Submit or simulate ---
        let (order_id, success, error_msg, making, taking) = if !self.paper_mode {
            match self.sign_and_submit(&token_id, &order, SdkSide::Buy).await {
                Ok(resp) => (
                    resp.order_id,
                    resp.success,
                    resp.error_msg,
                    resp.making_amount,
                    resp.taking_amount,
                ),
                Err(e) => {
                    error!("Buy failed: {}", e);
                    return None;
                }
            }
        } else {
            (
                "PAPERTRADE-1".to_string(),
                true,
                None,
                Decimal::default(),
                Decimal::default(),
            )
        };

        // --- Resolve fill price/size ---
        let (price, size) = if self.paper_mode {
            match order {
                OrderParams::Market { amount, .. } => {
                    let ask = match direction {
                        TokenDirection::Up => market.up.best_ask,
                        TokenDirection::Down => market.down.best_ask,
                    };
                    if ask > 0.0 {
                        (ask, amount / ask)
                    } else {
                        order.price_and_size()
                    }
                }
                OrderParams::Limit { price, size, .. } => (price, size),
            }
        } else if success && matches!(order, OrderParams::Market { .. }) {
            let making_f64: f64 = making.try_into().unwrap_or(0.0);
            let taking_f64: f64 = taking.try_into().unwrap_or(0.0);
            if taking_f64 > 0.0 {
                (making_f64 / taking_f64, taking_f64)
            } else {
                (0.0, 0.0)
            }
        } else {
            order.price_and_size()
        };

        // --- Update state ---
        if success {
            self.state = EngineState::InPosition;
        } else {
            let msg = error_msg.as_deref().unwrap_or_default();
            error!("Buy rejected: {}", msg);
        }

        Some(Trade {
            side: Side::Buy,
            direction,
            price,
            size,
            order_params: order,
            order_id,
            success,
            error_msg,
        })
    }

    // -----------------------------------------------------------------------
    // SDK order signing
    // -----------------------------------------------------------------------

    async fn sign_and_submit(
        &self,
        token_id: &str,
        order: &OrderParams,
        side: SdkSide,
    ) -> Result<PostOrderResponse, String> {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return Err("client not initialized".into());
        };

        let signable = match order {
            OrderParams::Limit {
                price,
                size,
                order_type,
            } => {
                let price_dec =
                    Decimal::from_str(&format!("{:.2}", price)).unwrap_or(Decimal::ZERO);
                let size_dec = Decimal::from_str(&format!("{:.2}", size)).unwrap_or(Decimal::ZERO);
                if price_dec <= Decimal::ZERO || size_dec <= Decimal::ZERO {
                    return Err(format!("Invalid price={} or size={}", price_dec, size_dec));
                }
                let sdk_order_type = match order_type {
                    MarketOrderType::FAK => OrderType::FAK,
                    MarketOrderType::FOK => OrderType::FOK,
                };
                client
                    .limit_order()
                    .order_type(sdk_order_type)
                    .token_id(token_id)
                    .side(side)
                    .price(price_dec)
                    .size(size_dec)
                    .build()
                    .await
            }
            OrderParams::Market { amount, order_type } => {
                let amount_dec =
                    Decimal::from_str(&format!("{:.2}", amount)).unwrap_or(Decimal::ZERO);
                if amount_dec <= Decimal::ZERO {
                    return Err(format!("Invalid amount={}", amount_dec));
                }
                let amt = if side == SdkSide::Buy {
                    Amount::usdc(amount_dec)
                } else {
                    Amount::shares(amount_dec)
                }
                .map_err(|e| format!("Invalid amount: {:?}", e))?;

                let sdk_order_type = match order_type {
                    MarketOrderType::FAK => OrderType::FAK,
                    MarketOrderType::FOK => OrderType::FOK,
                };
                client
                    .market_order()
                    .order_type(sdk_order_type)
                    .token_id(token_id)
                    .side(side)
                    .amount(amt)
                    .build()
                    .await
            }
        };

        let order = signable.map_err(|e| format!("Order build failed: {:?}", e))?;
        let signed_order = client
            .sign(signer, order)
            .await
            .map_err(|e| format!("Order sign failed: {:?}", e))?;
        client
            .post_order(signed_order)
            .await
            .map_err(|e| format!("Order post failed: {:?}", e))
    }
}
