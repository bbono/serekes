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

/// Convert a decimal string to a 64-char (256-bit) hex string.
fn decimal_to_hex256(decimal: &str) -> Option<String> {
    let mut digits: Vec<u8> = decimal.bytes().map(|b| b - b'0').collect();
    if digits.iter().any(|&d| d > 9) {
        return None;
    }
    let mut hex_chars = Vec::new();
    while digits.iter().any(|&d| d != 0) {
        let mut remainder = 0u32;
        for d in digits.iter_mut() {
            let cur = remainder * 10 + (*d as u32);
            *d = (cur / 16) as u8;
            remainder = cur % 16;
        }
        hex_chars.push(char::from_digit(remainder, 16).unwrap());
    }
    hex_chars.reverse();
    let hex = hex_chars.into_iter().collect::<String>();
    Some(format!("{:0>64}", hex))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineState {
    Idle,
    PendingBuy,
    InPosition,
    PendingSell,
}

#[allow(dead_code)]
pub struct StrategyEngine<S: Strategy> {
    pub strategy: S,
    pub trading_enabled: bool,
    pub asset: String,
    pub time_offset: i64,
    pub polygon_rpc_url: String,
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
    pub pending_since: i64,

    // Loop counters
    pub last_oracle_log: i64,
    pub last_recon_ms: i64,
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
        polygon_rpc_url: String,
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
            polygon_rpc_url,
            engine_config,
            client: None,
            signer_instance: None,
            state: EngineState::Idle,
            active_token_id: None,
            active_direction: None,
            entry_price: 0.0,
            position_size: 0.0,
            pending_since: 0,
            last_oracle_log: 0,
            last_recon_ms: 0,
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
        let now_sec = now_ms / 1000;
        let market = self.shared_market.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let exchange_divergence_pct = if coinbase_price > 0.0 {
            ((binance_price - coinbase_price) / coinbase_price * 100.0).abs()
        } else {
            0.0
        };

        // KILLSWITCH
        if exchange_divergence_pct > self.engine_config.exchange_divergence_threshold
            && coinbase_price > 0.0
            && binance_price > 0.0
        {
            let log_interval_ms = (self.engine_config.log_interval_secs * 1000.0) as i64;
            if now_ms - self.last_oracle_log >= log_interval_ms {
                self.last_oracle_log = now_ms;
                println!("[ENGINE] Killswitch - Market De-Peg! Binance: ${:.2} | Coinbase: ${:.2} | Divergence: {:.2}%",
                    binance_price, coinbase_price, exchange_divergence_pct);
            }
            return;
        }

        // STATE MACHINE LOCK CHECK & TIMEOUT
        if matches!(
            self.state,
            EngineState::PendingBuy | EngineState::PendingSell
        ) {
            // If market expired while pending, force reset to Idle
            if let Some(ref market) = market {
                if market.expires_at_ms <= now_ms {
                    eprintln!("[ENGINE] Market expired while in {:?}. Resetting to Idle.", self.state);
                    self.state = EngineState::Idle;
                    self.position_size = 0.0;
                    self.entry_price = 0.0;
                    self.active_token_id = None;
                    self.active_direction = None;
                    self.pending_since = 0;
                    return;
                }
            }
            if self.trading_enabled
                && (now_ms - self.last_recon_ms > 2000 || now_ms - self.pending_since > 5000)
            {
                self.last_recon_ms = now_ms;
                self.reconcile_with_chain().await;
            }
            return;
        }

        // IN-POSITION CHECKS
        if self.state == EngineState::InPosition {
            if let Some(ref market) = market {
                if let Some(token_id) = self.active_token_id.clone() {
                    // Verify our token belongs to this market
                    if market.up.token_id == token_id || market.down.token_id == token_id {
                        // Engine-level: expiry settlement
                        let time_to_expiry = market.expires_at_ms - now_ms;
                        if time_to_expiry <= 0 {
                            if !binance_price.is_finite() || !market.strike_price_binance.is_finite() {
                                eprintln!("[ENGINE] Invalid prices at expiry. binance={} strike={}", binance_price, market.strike_price_binance);
                                return;
                            }
                            let direction = self.active_direction.unwrap_or(TokenDirection::Up);
                            let settlement = match direction {
                                TokenDirection::Up => {
                                    if binance_price > market.strike_price_binance {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                                TokenDirection::Down => {
                                    if binance_price <= market.strike_price_binance {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                            };

                            let pnl = (settlement - self.entry_price) * self.position_size;
                            println!("[ENGINE] Settled at ${:.2} | PnL: ${:.4}", settlement, pnl);

                            if settlement > 0.5 {
                                let msg = format!("WIN! PnL: ${:.4}", pnl);
                                telegram::send_alert(&msg);
                            } else {
                                telegram::send_alert("LOSS. Position expired OTM.");
                            }

                            self.state = EngineState::Idle;
                            self.position_size = 0.0;
                            self.entry_price = 0.0;
                            self.active_token_id = None;
                            self.active_direction = None;
                            return;
                        }

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

            if self.trading_enabled && now_ms - self.last_recon_ms > 15000 {
                self.last_recon_ms = now_ms;
                self.reconcile_with_chain().await;
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
            self.state = EngineState::PendingBuy;
            self.active_token_id = Some(token_id.to_string());
            self.active_direction = Some(direction);
            self.pending_since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
                + (self.time_offset * 1000);
            self.entry_price = entry_price;
            self.position_size = size;

            if self.sign_and_submit(token_id, order, Side::Buy).await {
                self.traded_slugs.insert(slug.to_string());
                println!("[ENGINE] Buy Order Submitted: {}@${:.4}", size, entry_price);
                let msg = format!("TRADE PLACED! {} @ ${:.4}", self.asset, entry_price);
                telegram::send_alert(&msg);
            } else {
                self.state = EngineState::Idle;
                self.active_token_id = None;
                self.active_direction = None;
                self.pending_since = 0;
                self.entry_price = 0.0;
                self.position_size = 0.0;
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
            self.state = EngineState::PendingSell;
            self.pending_since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
                + (self.time_offset * 1000);
            if self.sign_and_submit(token_id, order, Side::Sell).await {
                println!("[ENGINE] Sell Order Submitted: {}@${:.4}", size, sell_price);
            } else {
                println!("[ENGINE] Sell failed. Reverting to InPosition.");
                self.state = EngineState::InPosition;
                self.pending_since = 0;
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

    pub async fn reconcile_with_chain(&mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let position_fut = async {
                if let Some(ref token_id) = self.active_token_id {
                    return self.check_on_chain_position(token_id).await;
                }
                None
            };

            let position_result = position_fut.await;

            if self.active_token_id.is_some() {
                if let Some(on_chain_shares) = position_result {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64
                        + (self.time_offset * 1000);

                    if self.state == EngineState::PendingBuy {
                        if on_chain_shares > 0.01 {
                            println!("[ENGINE] Buy confirmed on-chain.");
                            self.state = EngineState::InPosition;
                            self.position_size = on_chain_shares;
                            self.pending_since = 0;
                        } else if now_ms - self.pending_since > 5000 {
                            println!("[ENGINE] Buy failed/timeout. Reverting to Idle.");
                            self.state = EngineState::Idle;
                            self.active_token_id = None;
                            self.active_direction = None;
                            self.entry_price = 0.0;
                            self.position_size = 0.0;
                            self.pending_since = 0;
                        }
                    } else if self.state == EngineState::PendingSell {
                        if on_chain_shares < 0.01 {
                            println!("[ENGINE] Sell confirmed on-chain.");
                            let msg = "Sold.".to_string();
                            telegram::send_alert(&msg);
                            self.state = EngineState::Idle;
                            self.active_token_id = None;
                            self.active_direction = None;
                            self.entry_price = 0.0;
                            self.position_size = 0.0;
                            self.pending_since = 0;
                        } else if now_ms - self.pending_since > 5000 {
                            println!("[ENGINE] Sell failed/timeout. Reverting to InPosition.");
                            self.state = EngineState::InPosition;
                            self.position_size = on_chain_shares;
                            self.pending_since = 0;
                        }
                    } else if (on_chain_shares - self.position_size).abs() > 0.01 {
                        println!(
                            "[ENGINE] Share drift detected. Engine: {:.2}, Chain: {:.2}",
                            self.position_size, on_chain_shares
                        );
                        self.position_size = on_chain_shares;
                        if on_chain_shares < 0.01 {
                            self.state = EngineState::Idle;
                            self.active_token_id = None;
                            self.active_direction = None;
                            self.entry_price = 0.0;
                            self.pending_since = 0;
                        } else {
                            self.state = EngineState::InPosition;
                        }
                    }
                }
            }
        })
        .await;
    }

    async fn check_on_chain_position(&self, token_id: &str) -> Option<f64> {
        let signer = self.signer_instance.as_ref()?;
        let addr = alloy_signer::Signer::address(signer);
        let addr_hex = format!("{:x}", addr);
        let hex_token = decimal_to_hex256(token_id)?;
        let data = format!(
            "0x00fdd58e000000000000000000000000{}{}",
            addr_hex, hex_token
        );
        crate::rpc_eth_call(
            &self.polygon_rpc_url,
            "0x4D97d6599A46602052E175369CeBa61a5b8cae6a",
            &data,
        )
        .await
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
