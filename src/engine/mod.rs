pub mod strategies;
pub mod traits;

pub use strategies::BonoStrategy;
#[allow(unused_imports)]
pub use traits::{OrderParams, Strategy, TickContext};

use crate::config::EngineConfig;
use crate::telegram;
use crate::types::{Market, TokenDirection};
use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as SDKSigner};
use polymarket_client_sdk::clob::types::{Amount, Side};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::POLYGON;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

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
    pub client: Option<ClobClient<Authenticated<Normal>>>,
    pub signer_instance: Option<PrivateKeySigner>,

    // State machine
    pub state: EngineState,
    pub active_token_id: Option<String>,
    pub active_direction: Option<TokenDirection>,
    pub entry_price: f64,
    pub position_size: f64,

    // Loop counters
    pub last_oracle_log: i64,
    pub last_killswitch_log: i64,
    pub traded_slugs: std::collections::HashSet<String>,

    // Watch Receivers
    pub binance_rx: watch::Receiver<(f64, i64)>,
    pub coinbase_rx: watch::Receiver<(f64, i64)>,
    pub chainlink_rx: watch::Receiver<(f64, i64)>,
    pub dvol_rx: watch::Receiver<f64>,
    pub shared_market: Arc<Mutex<Option<Market>>>,
}

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
            active_token_id: None,
            active_direction: None,
            entry_price: 0.0,
            position_size: 0.0,
            last_oracle_log: 0,
            last_killswitch_log: 0,
            traded_slugs: std::collections::HashSet::new(),
            binance_rx,
            coinbase_rx,
            chainlink_rx,
            dvol_rx,
            shared_market,
        }
    }

    pub async fn execute_tick(&mut self) {
        let (binance_price, binance_ts) = *self.binance_rx.borrow();
        let (coinbase_price, coinbase_ts) = *self.coinbase_rx.borrow();
        let (chainlink_price, chainlink_ts) = *self.chainlink_rx.borrow();
        let dvol = *self.dvol_rx.borrow();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            + (self.time_offset * 1000);
        let market = self.shared_market.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let exchange_divergence = (binance_price - coinbase_price).abs();

        // KILLSWITCH
        if exchange_divergence > self.engine_config.exchange_price_divergence_threshold
            && coinbase_price > 0.0
            && binance_price > 0.0
        {
            let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
            if now_ms - self.last_killswitch_log >= log_interval_ms {
                self.last_killswitch_log = now_ms;
                println!("[ENGINE] Killswitch - Market De-Peg! Binance: ${:.2} | Coinbase: ${:.2} | Divergence: ${:.2}",
                    binance_price, coinbase_price, exchange_divergence);
            }
            return;
        }

        // Skip strategy checks until Polymarket price feed has populated both sides
        let market_ready = market
            .as_ref()
            .map_or(false, |m| m.up.last_trade_price > 0.0 && m.down.last_trade_price > 0.0);

        if !market_ready {
            return;
        }

        // IN-POSITION CHECKS
        if self.state == EngineState::InPosition {
            if let Some(ref market) = market {
                if let Some(token_id) = self.active_token_id.clone() {
                    // Verify our token belongs to this market
                    if market.up.token_id == token_id || market.down.token_id == token_id {
                        // Strategy-level exit
                        let ctx = TickContext {
                            binance_price,
                            binance_ts,
                            coinbase_price,
                            coinbase_ts,
                            chainlink_price,
                            chainlink_ts,
                            dvol,
                            now_ms,
                        };
                        if let Some(order) =
                            self.strategy.check_exit(&ctx, market, self.position_size)
                        {
                            self.execute_sell(&token_id, &order).await;
                            return;
                        }
                    }
                }
            }
        }

        // ENTRY MATRIX
        if self.state == EngineState::Idle {
            if let Some(ref market) = market {
                if !self.traded_slugs.contains(&market.slug) {
                    let ctx = TickContext {
                        binance_price,
                        binance_ts,
                        coinbase_price,
                        coinbase_ts,
                        chainlink_price,
                        chainlink_ts,
                        dvol,
                        now_ms,
                    };

                    if let Some((direction, order)) = self.strategy.check_entry(&ctx, market) {
                        let token_id = match direction {
                            TokenDirection::Up => market.up.token_id.clone(),
                            TokenDirection::Down => market.down.token_id.clone(),
                        };
                        self.execute_buy(&token_id, direction, &market.slug, &order)
                            .await;
                    }
                }
            }
        }

        let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
        if now_ms - self.last_oracle_log >= log_interval_ms {
            self.last_oracle_log = now_ms;
            if self.state != EngineState::Idle {
                println!("[ENGINE] {:?} | entry ${:.4} | size {:.2}", self.state, self.entry_price, self.position_size);
            }
        }
    }

    async fn execute_buy(
        &mut self,
        token_id: &str,
        direction: TokenDirection,
        slug: &str,
        order: &OrderParams,
    ) {
        let (entry_price, size) = order.price_and_size();

        if self.trading_enabled {
            if self.sign_and_submit(token_id, order, Side::Buy).await {
                self.state = EngineState::InPosition;
                self.active_token_id = Some(token_id.to_string());
                self.active_direction = Some(direction);
                self.entry_price = entry_price;
                self.position_size = size;
                self.traded_slugs.insert(slug.to_string());
                println!("[ENGINE] Buy Order Submitted: {}@${:.4}", size, entry_price);
                let msg = format!("TRADE PLACED! {} @ ${:.4}", self.asset, entry_price);
                telegram::send_alert(&msg);
            }
        } else {
            self.state = EngineState::InPosition;
            self.position_size = size;
            self.entry_price = entry_price;
            self.active_token_id = Some(token_id.to_string());
            self.active_direction = Some(direction);
            self.traded_slugs.insert(slug.to_string());
            println!("[ENGINE] Simulated Entry at ${:.4}", entry_price);
            let msg = format!("TRADE PLACED! {} @ ${:.4}", self.asset, entry_price);
            telegram::send_alert(&msg);
        }
    }

    async fn execute_sell(&mut self, token_id: &str, order: &OrderParams) {
        let (sell_price, size) = order.price_and_size();

        if self.trading_enabled {
            if self.sign_and_submit(token_id, order, Side::Sell).await {
                let pnl = (sell_price - self.entry_price) * size;
                self.state = EngineState::Idle;
                self.position_size = 0.0;
                self.active_token_id = None;
                self.active_direction = None;
                println!("[ENGINE] Sell Order Submitted: {}@${:.4} | PnL: ${:.4}", size, sell_price, pnl);
            } else {
                println!("[ENGINE] Sell failed. Staying InPosition.");
            }
        } else {
            let pnl = (sell_price - self.entry_price) * size;
            self.state = EngineState::Idle;
            self.position_size = 0.0;
            self.active_token_id = None;
            self.active_direction = None;
            println!("[ENGINE] Simulated exit at ${:.4} | PnL: ${:.4}", sell_price, pnl);
        }
    }

    async fn sign_and_submit(&self, token_id: &str, order: &OrderParams, side: Side) -> bool {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return false;
        };

        let signable = match order {
            OrderParams::Limit { price, size } => {
                let price_dec =
                    Decimal::from_str(&format!("{:.2}", price)).unwrap_or(Decimal::ZERO);
                let size_dec = Decimal::from_str(&format!("{:.2}", size)).unwrap_or(Decimal::ZERO);
                if price_dec <= Decimal::ZERO || size_dec <= Decimal::ZERO {
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
            OrderParams::Market { amount } => {
                let amount_dec =
                    Decimal::from_str(&format!("{:.2}", amount)).unwrap_or(Decimal::ZERO);
                if amount_dec <= Decimal::ZERO {
                    return false;
                }

                let amt = if side == Side::Buy {
                    Amount::usdc(amount_dec)
                } else {
                    Amount::shares(amount_dec)
                };
                let amt = match amt {
                    Ok(a) => a,
                    Err(_) => return false,
                };

                client
                    .market_order()
                    .token_id(token_id)
                    .amount(amt)
                    .side(side)
                    .build()
                    .await
            }
        };

        if let Ok(order) = signable {
            if let Ok(signed_order) = client.sign(signer, order).await {
                if let Ok(resp) = client.post_order(signed_order).await {
                    println!("[ENGINE] Order Submitted! ID: {:?}", resp.order_id);
                    return true;
                }
            }
        }
        false
    }

    pub async fn initialize_client(&mut self, private_key: &str) -> bool {
        let signer: PrivateKeySigner = LocalSigner::from_str(private_key)
            .unwrap()
            .with_chain_id(Some(POLYGON));
        let client_builder =
            ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();

        match client_builder
            .authentication_builder(&signer)
            .authenticate()
            .await
        {
            Ok(client) => {
                self.client = Some(client);
                self.signer_instance = Some(signer);
                true
            }
            Err(e) => {
                eprintln!("[ENGINE] SDK Auth failed: {}", e);
                false
            }
        }
    }
}
