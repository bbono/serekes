use crate::common::config::AppConfig;
use crate::common::types::{
    Market, OrderIntent, Strategy, TickContext, TokenDirection, Trade,
};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{error, info, warn};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as _};
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{Amount, OrderStatusType, SignatureType};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::POLYGON;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

pub struct StrategyEngine {
    strategy: Box<dyn Strategy>,
    pub paper_mode: bool,
    pub config: AppConfig,

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

// ---------------------------------------------------------------------------
// Constructor & auth
// ---------------------------------------------------------------------------

impl StrategyEngine {
    pub fn new(
        strategy: Box<dyn Strategy>,
        paper_mode: bool,
        config: AppConfig,
        binance_rx: watch::Receiver<(f64, i64)>,
        coinbase_rx: watch::Receiver<(f64, i64)>,
        chainlink_rx: watch::Receiver<(f64, i64)>,
        dvol_rx: watch::Receiver<(f64, i64)>,
        shared_market: Arc<Mutex<Option<Market>>>,
        binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
        dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    ) -> Self {
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

    /// Resets per-market state. Call between market rotations.
    pub fn clear_state(&mut self) {
        self.trades.clear();
        self.last_log_ts = 0;
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

        // 2. Try to place an order
        self.try_order(&ctx).await
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
    // Tick helpers
    // -----------------------------------------------------------------------

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

    async fn try_order(&mut self, ctx: &TickContext) -> Option<Trade> {
        let market = ctx.market.as_ref()?;
        let (direction, intent) = self.strategy.create_order(ctx)?;

        // --- Validate minimum order size ---
        if market.min_order_size > 0.0 {
            let (_, size) = intent.price_and_size();
            if size < market.min_order_size {
                warn!(
                    "Order size {:.4} below minimum {:.4}. Skipping.",
                    size, market.min_order_size
                );
                return None;
            }
        }

        let token_id = match direction {
            TokenDirection::Up => market.up.token_id.clone(),
            TokenDirection::Down => market.down.token_id.clone(),
        };

        // --- Submit or simulate ---
        let (order_id, order_status, making, taking): (String, OrderStatusType, Decimal, Decimal) =
            if !self.paper_mode {
                match self.sign_and_submit(&token_id, &intent).await {
                    Ok(resp) => {
                        match &resp.status {
                            OrderStatusType::Matched => {
                                info!("Order {} matched immediately", resp.order_id);
                            }
                            OrderStatusType::Delayed => {
                                warn!("Order {} delayed by matching engine", resp.order_id);
                            }
                            OrderStatusType::Unmatched => {
                                warn!("Order {} unmatched (placement ok, no fill)", resp.order_id);
                            }
                            OrderStatusType::Live => {
                                info!("Order {} resting on book", resp.order_id);
                            }
                            _ => {
                                warn!(
                                    "Order {} unexpected status: {:?}",
                                    resp.order_id, resp.status
                                );
                            }
                        }
                        (
                            resp.order_id,
                            resp.status,
                            resp.making_amount,
                            resp.taking_amount,
                        )
                    }
                    Err(e) => {
                        error!("Order failed: {}", e);
                        return None;
                    }
                }
            } else {
                (
                    "PAPERTRADE-1".to_string(),
                    OrderStatusType::Matched,
                    Decimal::default(),
                    Decimal::default(),
                )
            };

        // --- Resolve fill price/size ---
        let (price, size) = if self.paper_mode {
            match &intent {
                OrderIntent::Market { amount, .. } => {
                    let ask = match direction {
                        TokenDirection::Up => market.up.best_ask,
                        TokenDirection::Down => market.down.best_ask,
                    };
                    let amt_f64: f64 = (*amount).try_into().unwrap_or(0.0);
                    if ask > 0.0 {
                        (ask, amt_f64 / ask)
                    } else {
                        intent.price_and_size()
                    }
                }
                OrderIntent::Limit { price, size, .. } => {
                    let p: f64 = (*price).try_into().unwrap_or(0.0);
                    let s: f64 = (*size).try_into().unwrap_or(0.0);
                    (p, s)
                }
            }
        } else if order_status == OrderStatusType::Matched
            && matches!(intent, OrderIntent::Market { .. })
        {
            let making_f64: f64 = making.try_into().unwrap_or(0.0);
            let taking_f64: f64 = taking.try_into().unwrap_or(0.0);
            if taking_f64 > 0.0 {
                (making_f64 / taking_f64, taking_f64)
            } else {
                (0.0, 0.0)
            }
        } else {
            intent.price_and_size()
        };

        let trade = Trade {
            direction,
            intent,
            price,
            size,
            order_id,
            order_status,
            timestamp_ms: crate::common::time::now_ms(),
        };
        self.trades.push(trade.clone());
        Some(trade)
    }

    // -----------------------------------------------------------------------
    // SDK order signing
    // -----------------------------------------------------------------------

    async fn sign_and_submit(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<PostOrderResponse, String> {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return Err("client not initialized".into());
        };

        let token_u256 =
            U256::from_str(token_id).map_err(|e| format!("Invalid token_id: {:?}", e))?;

        let order = match intent {
            OrderIntent::Limit {
                side,
                price,
                size,
                order_type,
            } => client
                .limit_order()
                .order_type(order_type.clone())
                .token_id(token_u256)
                .side(*side)
                .price(*price)
                .size(*size)
                .build()
                .await
                .map_err(|e| format!("Order build failed: {:?}", e))?,
            OrderIntent::Market {
                side,
                amount,
                order_type,
            } => {
                let amt = if *side == polymarket_client_sdk::clob::types::Side::Buy {
                    Amount::usdc(*amount)
                } else {
                    Amount::shares(*amount)
                }
                .map_err(|e| format!("Invalid amount: {:?}", e))?;

                client
                    .market_order()
                    .order_type(order_type.clone())
                    .token_id(token_u256)
                    .side(*side)
                    .amount(amt)
                    .build()
                    .await
                    .map_err(|e| format!("Order build failed: {:?}", e))?
            }
        };

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
