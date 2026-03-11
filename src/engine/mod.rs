pub mod strategies;
pub mod traits;

pub use strategies::BonoStrategy;
#[allow(unused_imports)]
pub use traits::{OrderParams, Strategy, TickContext};

use crate::config::EngineConfig;
use crate::telegram;
use crate::types::{Market, TokenDirection};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{error, info, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as SDKSigner};
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side, SignatureType};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::POLYGON;
use std::str::FromStr;
use std::collections::VecDeque;
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

#[allow(dead_code)]
pub struct StrategyEngine<S: Strategy> {
    pub strategy: S,
    pub trading_enabled: bool,
    pub asset: String,
    pub time_offset: i64,
    pub engine_config: EngineConfig,

    // Auth / SDK
    client: Option<ClobClient<Authenticated<Normal>>>,
    signer_instance: Option<PrivateKeySigner>,

    // State machine
    state: EngineState,
    entry_failed: bool,
    active_token_id: Option<String>,
    active_direction: Option<TokenDirection>,
    entry_price: f64,
    position_size: f64,

    // Loop counters
    last_oracle_log: i64,
    last_killswitch_log: i64,

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
        trading_enabled: bool,
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
            trading_enabled,
            asset,
            time_offset,
            engine_config,
            client: None,
            signer_instance: None,
            state: EngineState::Idle,
            entry_failed: false,
            active_token_id: None,
            active_direction: None,
            entry_price: 0.0,
            position_size: 0.0,
            last_oracle_log: 0,
            last_killswitch_log: 0,
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
    pub async fn execute_tick(&mut self) -> bool {
        let ctx = self.snapshot();
        let market = self
            .shared_market
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // 1. If in position and strategy manages exits — check exit (even during divergence)
        if self.state == EngineState::InPosition && self.strategy.manages_exit() {
            self.try_exit(&ctx, &market).await;
            // Just exited → done
            if self.state == EngineState::Idle {
                return true;
            }
        }

        // 2. Killswitch — block new entries if exchanges diverge
        if self.check_killswitch(&ctx) {
            return false;
        }

        // 3. If idle and haven't already failed — check entry
        if self.state == EngineState::Idle && !self.entry_failed {
            self.try_entry(&ctx, &market).await;
            // Entry failed → done
            if self.entry_failed {
                return true;
            }
            // Just entered and strategy doesn't manage exits → done
            if self.state == EngineState::InPosition && !self.strategy.manages_exit() {
                self.clear_position();
                return true;
            }
        }

        // 4. Periodic status log
        self.log_status(&ctx);
        false
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

        TickContext {
            binance_price,
            binance_ts,
            coinbase_price,
            coinbase_ts,
            chainlink_price,
            chainlink_ts,
            dvol,
            now_ms,
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
            if ctx.now_ms - self.last_killswitch_log >= log_interval_ms {
                self.last_killswitch_log = ctx.now_ms;
                warn!(
                    "Killswitch! Binance=${:.2} Coinbase=${:.2} divergence=${:.2}",
                    ctx.binance_price, ctx.coinbase_price, divergence
                );
            }
            return true;
        }
        false
    }

    async fn try_exit(&mut self, ctx: &TickContext, market: &Option<Market>) {
        let Some(ref market) = market else { return };
        let Some(ref token_id) = self.active_token_id.clone() else {
            return;
        };
        if market.up.token_id != *token_id && market.down.token_id != *token_id {
            return;
        }
        if let Some(order) = self.strategy.check_exit(ctx, market, self.position_size) {
            self.execute_sell(token_id, &order).await;
        }
    }

    async fn try_entry(&mut self, ctx: &TickContext, market: &Option<Market>) {
        let Some(ref market) = market else { return };
        if let Some((direction, order)) = self.strategy.check_entry(ctx, market) {
            let token_id = match direction {
                TokenDirection::Up => market.up.token_id.clone(),
                TokenDirection::Down => market.down.token_id.clone(),
            };
            self.execute_buy(&token_id, direction, &order).await;
        }
    }

    fn log_status(&mut self, ctx: &TickContext) {
        let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
        if ctx.now_ms - self.last_oracle_log >= log_interval_ms {
            self.last_oracle_log = ctx.now_ms;
            if self.state != EngineState::Idle {
                info!(
                    "{:?} | entry=${:.4} size={:.2}",
                    self.state, self.entry_price, self.position_size
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Order execution
    // -----------------------------------------------------------------------

    async fn execute_buy(
        &mut self,
        token_id: &str,
        direction: TokenDirection,
        order: &OrderParams,
    ) {
        let (entry_price, size) = order.price_and_size();

        let submitted = if self.trading_enabled {
            self.sign_and_submit(token_id, order, Side::Buy).await
        } else {
            true
        };

        if !submitted {
            self.entry_failed = true;
            return;
        }

        self.enter_position(token_id, direction, entry_price, size);

        if self.trading_enabled {
            info!("Buy submitted: {}@${:.4}", size, entry_price);
        } else {
            info!("Simulated entry at ${:.4}", entry_price);
        }
        telegram::send_alert(&format!(
            "TRADE PLACED! {} @ ${:.4}",
            self.asset, entry_price
        ));
    }

    async fn execute_sell(&mut self, token_id: &str, order: &OrderParams) {
        let (sell_price, size) = order.price_and_size();

        let submitted = if self.trading_enabled {
            self.sign_and_submit(token_id, order, Side::Sell).await
        } else {
            true
        };

        if !submitted {
            warn!("Sell failed. Staying InPosition.");
            return;
        }

        let pnl = (sell_price - self.entry_price) * size;
        self.clear_position();

        if self.trading_enabled {
            info!("Sell submitted: {}@${:.4} PnL=${:.4}", size, sell_price, pnl);
        } else {
            info!("Simulated exit at ${:.4} PnL=${:.4}", sell_price, pnl);
        }
    }

    // -----------------------------------------------------------------------
    // Position state helpers
    // -----------------------------------------------------------------------

    fn enter_position(
        &mut self,
        token_id: &str,
        direction: TokenDirection,
        entry_price: f64,
        size: f64,
    ) {
        self.state = EngineState::InPosition;
        self.active_token_id = Some(token_id.to_string());
        self.active_direction = Some(direction);
        self.entry_price = entry_price;
        self.position_size = size;
    }

    fn clear_position(&mut self) {
        self.state = EngineState::Idle;
        self.entry_failed = false;
        self.active_token_id = None;
        self.active_direction = None;
        self.entry_price = 0.0;
        self.position_size = 0.0;
    }

    // -----------------------------------------------------------------------
    // SDK order signing
    // -----------------------------------------------------------------------

    async fn sign_and_submit(&self, token_id: &str, order: &OrderParams, side: Side) -> bool {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            error!("Order failed: client not initialized");
            return false;
        };

        let signable = match order {
            OrderParams::Limit { price, size } => {
                let price_dec =
                    Decimal::from_str(&format!("{:.2}", price)).unwrap_or(Decimal::ZERO);
                let size_dec =
                    Decimal::from_str(&format!("{:.2}", size)).unwrap_or(Decimal::ZERO);
                if price_dec <= Decimal::ZERO || size_dec <= Decimal::ZERO {
                    error!("Order failed: invalid price={} or size={}", price_dec, size_dec);
                    return false;
                }
                client
                    .limit_order()
                    .token_id(token_id)
                    .price(price_dec)
                    .size(size_dec)
                    .side(side)
                    .build()
                    .await
            }
            OrderParams::Market { amount, price, order_type } => {
                let amount_dec =
                    Decimal::from_str(&format!("{:.2}", amount)).unwrap_or(Decimal::ZERO);
                if amount_dec <= Decimal::ZERO {
                    error!("Order failed: invalid amount={}", amount_dec);
                    return false;
                }
                let amt = if side == Side::Buy {
                    Amount::usdc(amount_dec)
                } else {
                    Amount::shares(amount_dec)
                };
                let amt = match amt {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Order failed: invalid amount: {:?}", e);
                        return false;
                    }
                };
                let sdk_order_type = match order_type {
                    traits::MarketOrderType::FAK => OrderType::FAK,
                    traits::MarketOrderType::FOK => OrderType::FOK,
                };
                let mut builder = client
                    .market_order()
                    .token_id(token_id)
                    .amount(amt)
                    .side(side)
                    .order_type(sdk_order_type);
                if let Some(p) = price {
                    let price_dec =
                        Decimal::from_str(&format!("{:.4}", p)).unwrap_or(Decimal::ZERO);
                    if price_dec > Decimal::ZERO {
                        builder = builder.price(price_dec);
                    }
                }
                builder.build().await
            }
        };

        let order = match signable {
            Ok(o) => o,
            Err(e) => {
                error!("Order build failed: {:?}", e);
                return false;
            }
        };
        let signed_order = match client.sign(signer, order).await {
            Ok(s) => s,
            Err(e) => {
                error!("Order sign failed: {:?}", e);
                return false;
            }
        };
        match client.post_order(signed_order).await {
            Ok(resp) => {
                info!("Order submitted: id={:?}", resp.order_id);
                true
            }
            Err(e) => {
                error!("Order post failed: {:?}", e);
                false
            }
        }
    }
}
