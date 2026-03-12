pub mod strategies;
pub mod traits;

pub use strategies::BonoStrategy;
#[allow(unused_imports)]
pub use traits::{OrderParams, Strategy, TickContext};

use crate::config::EngineConfig;
use crate::types::{Market, TokenDirection};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{error, info, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as SDKSigner};
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::POLYGON;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

#[derive(Debug)]
#[allow(dead_code)]
pub struct ExecuteOrderResponse {
    pub side: Side,
    pub direction: TokenDirection,
    pub price: f64,
    pub size: f64,
    pub order_params: OrderParams,
    pub order_id: String,
    pub success: bool,
    pub error_msg: Option<String>,
}

#[allow(dead_code)]
pub struct ExecuteTickResult {
    pub state: EngineState,
    pub order: Option<ExecuteOrderResponse>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineState {
    Idle,
    InPosition,
}

#[allow(dead_code)]
pub struct StrategyEngine<S: Strategy> {
    pub strategy: S,
    pub paper_mode: bool,
    pub asset: String,
    pub time_offset: i64,
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
        asset: String,
        time_offset: i64,
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
            asset,
            time_offset,
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

    /// Returns `true` when the engine has reached its final state:
    /// - entry-only strategy placed a buy, or
    /// - entry+exit strategy completed the full round-trip.
    pub async fn execute_tick(&mut self) -> ExecuteTickResult {
        let ctx = self.snapshot();

        // 1. If in position and strategy manages exits — check exit (even during divergence)
        /* if self.state == EngineState::InPosition && self.strategy.manages_exit() {
            self.try_exit(&ctx).await;

            if self.state == EngineState::Idle {
                return;
            }
        } */

        // 2. Killswitch — block new entries if exchanges diverge
        if self.check_killswitch(&ctx) {
            return ExecuteTickResult {
                state: self.state,
                order: None,
            };
        }

        // 3. Try buy
        let order = if self.state == EngineState::Idle {
            self.try_buy(&ctx).await
        } else {
            None
        };

        let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
        if ctx.now_ms - self.last_engine_state_log_ts >= log_interval_ms {
            self.last_engine_state_log_ts = ctx.now_ms;
            info!("[{:?}]", self.state);
        }

        ExecuteTickResult {
            state: self.state,
            order,
        }
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
            + (self.time_offset * 1000);
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

    /* async fn try_exit(&mut self, ctx: &TickContext) {
        let Some(ref market) = ctx.market else { return };
        let Some(ref token_id) = self.active_token_id.clone() else {
            return;
        };
        let direction = if market.up.token_id == *token_id {
            TokenDirection::Up
        } else if market.down.token_id == *token_id {
            TokenDirection::Down
        } else {
            return;
        };
        if let Some(order) = self.strategy.create_exit_order(ctx, self.position_size) {
            match self.execute_sell(token_id, direction, &order).await {
                Ok(resp) => {
                    let success = resp.result.as_ref().map_or(true, |r| r.success);
                    if success {
                        self.clear_position();
                    } else {
                        let msg = resp.result.and_then(|r| r.error_msg).unwrap_or_default();
                        error!("Sell rejected: {}. Staying InPosition.", msg);
                    }
                }
                Err(e) => error!("Sell failed: {}. Staying InPosition.", e),
            }
        }
    } */

    async fn try_buy(
        &mut self,
        ctx: &TickContext,
    ) -> Option<ExecuteOrderResponse> {
        let market = ctx.market.as_ref()?;
        let (direction, order) = self.strategy.create_entry_order(ctx)?;
        let token_id = match direction {
            TokenDirection::Up => market.up.token_id.clone(),
            TokenDirection::Down => market.down.token_id.clone(),
        };

        // --- Submit or simulate ---
        let (order_id, success, error_msg, making, taking) = if !self.paper_mode {
            match self.sign_and_submit(&token_id, &order, Side::Buy).await {
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

        Some(ExecuteOrderResponse {
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

    /* async fn execute_sell(
        &mut self,
        token_id: &str,
        direction: TokenDirection,
        order: &OrderParams,
    ) -> Result<ExecuteOrderResponse, String> {
        let result = if !self.paper_mode {
            let resp = self.sign_and_submit(token_id, order, Side::Sell).await?;
            Some(ExecuteOrderResult {
                order_id: resp.order_id,
                success: resp.success,
                making_amount: resp.making_amount,
                taking_amount: resp.taking_amount,
                error_msg: resp.error_msg,
            })
        } else {
            None
        };

        Ok(ExecuteOrderResponse { side: Side::Sell, direction, order_params: order.clone(), result })
    } */

    // -----------------------------------------------------------------------
    // SDK order signing
    // -----------------------------------------------------------------------

    async fn sign_and_submit(
        &self,
        token_id: &str,
        order: &OrderParams,
        side: Side,
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
                    traits::MarketOrderType::FAK => OrderType::FAK,
                    traits::MarketOrderType::FOK => OrderType::FOK,
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
                let amt = if side == Side::Buy {
                    Amount::usdc(amount_dec)
                } else {
                    Amount::shares(amount_dec)
                }
                .map_err(|e| format!("Invalid amount: {:?}", e))?;

                let sdk_order_type = match order_type {
                    traits::MarketOrderType::FAK => OrderType::FAK,
                    traits::MarketOrderType::FOK => OrderType::FOK,
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
