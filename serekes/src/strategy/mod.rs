mod traits;
pub mod gap_arb;

pub use traits::{Strategy, TickContext, ExitReason, OrderParams};
pub use gap_arb::GapArbStrategy;

use crate::types::{Market, TokenDirection};
use crate::config::StrategyConfig;
use crate::telegram;
use ethers::prelude::*;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, Amount};
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Signer as SDKSigner, Normal};
use polymarket_client_sdk::POLYGON;
use alloy_signer_local::LocalSigner;
use k256::ecdsa::SigningKey;
use std::str::FromStr;
use std::collections::HashMap;
use chrono::Utc;
use tokio::sync::watch;

use crate::state::{PersistentState, BotState};
use crate::atr::AtrMonitor;
use crate::velocity::VelocityLockout;

pub struct StrategyEngine<S: Strategy> {
    pub strategy: S,
    pub trading_enabled: bool,
    pub asset: String,
    pub time_offset: i64,
    pub polygon_rpc_url: String,
    pub strategy_config: StrategyConfig,
    // Auth / SDK
    pub client: Option<ClobClient<Authenticated<Normal>>>,
    pub signer_instance: Option<LocalSigner<SigningKey>>,

    // Risk & Loop Counters
    pub atr: AtrMonitor,
    pub velocity: VelocityLockout,
    pub state: PersistentState,
    pub last_oracle_log: i64,
    pub last_recon_ms: i64,
    pub traded_markets: std::collections::HashSet<String>,

    // Watch Receivers
    pub binance_rx: watch::Receiver<f64>,
    pub coinbase_rx: watch::Receiver<f64>,
    pub dvol_rx: watch::Receiver<f64>,
    pub polymarket_rx: watch::Receiver<HashMap<String, Market>>,
    pub market_id_rx: watch::Receiver<Option<String>>,
}

impl<S: Strategy> StrategyEngine<S> {
    pub async fn new(
        strategy: S,
        trading_enabled: bool,
        asset: String,
        time_offset: i64,
        polygon_rpc_url: String,
        strategy_config: StrategyConfig,
        binance_rx: watch::Receiver<f64>,
        coinbase_rx: watch::Receiver<f64>,
        dvol_rx: watch::Receiver<f64>,
        polymarket_rx: watch::Receiver<HashMap<String, Market>>,
        market_id_rx: watch::Receiver<Option<String>>,
    ) -> Self {
        let state = PersistentState::load();
        let atr = AtrMonitor::new(&asset).await;
        let velocity = VelocityLockout::new(0.0003, 2);

        Self {
            strategy,
            trading_enabled,
            asset,
            time_offset,
            polygon_rpc_url,
            strategy_config,
            client: None,
            signer_instance: None,
            atr,
            velocity,
            state,
            last_oracle_log: 0,
            last_recon_ms: 0,
            traded_markets: std::collections::HashSet::new(),
            binance_rx,
            coinbase_rx,
            dvol_rx,
            polymarket_rx,
            market_id_rx,
        }
    }

    pub async fn execute_tick(&mut self, binance_price: f64, coinbase_price: f64) {
        let now_ms = Utc::now().timestamp_millis() + (self.time_offset * 1000);
        let now_sec = now_ms / 1000;
        let dvol = *self.dvol_rx.borrow();
        let active_dvol = if dvol > 0.0 { dvol } else { self.atr.current_atr() * 0.5 };
        let markets = self.polymarket_rx.borrow().clone();

        let exchange_divergence = (binance_price - coinbase_price).abs();

        // KILLSWITCH
        if exchange_divergence > self.strategy_config.killswitch_threshold && coinbase_price > 0.0 && binance_price > 0.0 {
            if now_sec % 5 == 0 {
                println!("[KILLSWITCH] Market De-Peg! Binance: ${:.2} | Coinbase: ${:.2} | Divergence: ${:.2}",
                    binance_price, coinbase_price, exchange_divergence);
            }
            return;
        }

        // STATE MACHINE LOCK CHECK & TIMEOUT
        if matches!(self.state.state, BotState::PendingBuy | BotState::PendingSell) {
            if now_ms - self.last_recon_ms > 2000 || now_ms - self.state.pending_since > 5000 {
                self.last_recon_ms = now_ms;
                self.reconcile_with_chain().await;
            }
            return;
        }

        // EMERGENCY EXITS
        if self.state.state == BotState::InPosition {
            if let Some(market_id) = &self.state.active_market_id.clone() {
                if let Some(market) = markets.get(market_id) {
                    // Engine-level: exchange divergence exit
                    let divergence_exit = coinbase_price > 0.0 && binance_price > 0.0
                        && (coinbase_price - binance_price).abs() > (binance_price * self.strategy_config.divergence_exit_pct / 100.0);
                    if divergence_exit {
                        println!("[EXIT] EMERGENCY EJECT: EXCHANGE DIVERGENCE");
                        self.emergency_sell(market).await;
                        return;
                    }

                    // Engine-level: expiry settlement
                    let time_to_expiry = market.expiration - (now_ms / 1000);
                    if time_to_expiry <= 0 {
                        let settlement = if market.direction == TokenDirection::Up {
                            if binance_price > market.strike_price { 1.0 } else { 0.0 }
                        } else {
                            if binance_price <= market.strike_price { 1.0 } else { 0.0 }
                        };

                        let pnl = (settlement - self.state.entry_price) * self.state.position_size;
                        println!("[EXPIRY] Settled at ${:.2} | PnL: ${:.4}", settlement, pnl);

                        if settlement > 0.5 {
                            let msg = format!("WIN! Balance: ${:.2}", self.state.simulated_balance + pnl);
                            telegram::send_alert(&msg);
                        } else {
                            telegram::send_alert("LOSS. Position expired OTM.");
                        }

                        if !self.trading_enabled {
                            self.state.simulated_balance += pnl;
                        }

                        self.state.state = BotState::Idle;
                        self.state.position_size = 0.0;
                        self.state.entry_price = 0.0;
                        self.state.active_market_id = None;
                        self.state.save();
                        return;
                    }

                    // Strategy-level exits
                    let ctx = TickContext {
                        binance_price,
                        coinbase_price,
                        dvol,
                        now_ms,
                    };

                    match self.strategy.check_exit(&ctx, market) {
                        Some(ExitReason::Emergency(reason)) => {
                            println!("[EXIT] EMERGENCY EJECT: {}", reason);
                            self.emergency_sell(market).await;
                            return;
                        }
                        None => {}
                    }
                }
            }

            if now_ms - self.last_recon_ms > 15000 {
                self.last_recon_ms = now_ms;
                self.reconcile_with_chain().await;
            }
        }

        self.atr.update_from_tick(binance_price, now_ms / 1000);
        self.velocity.update(binance_price);

        // ENTRY MATRIX
        if self.state.state == BotState::Idle {
            if self.velocity.is_locked() { return; }

            let ctx = TickContext {
                binance_price,
                coinbase_price,
                dvol,
                now_ms,
            };

            for market in markets.values() {
                if self.traded_markets.contains(&market.id) { continue; }

                // Platform guards
                let mkt_price = market.last_price;
                if mkt_price < 0.01 || mkt_price > 0.99 { continue; }
                let bid = market.orderbook.best_bid().unwrap_or(0.0);
                let ask = market.orderbook.best_ask().unwrap_or(1.0);
                if (ask - bid).abs() > self.strategy_config.max_spread { continue; }

                if let Some(order) = self.strategy.check_entry(&ctx, market, self.state.simulated_balance) {
                    self.execute_buy(market, &order).await;
                    break;
                }
            }
        }

        if now_sec - self.last_oracle_log >= self.strategy_config.log_interval_secs {
            self.last_oracle_log = now_sec;
            {
                let mut spread = 0.0;
                let mut mkt_price = 0.0;
                if let Some(m) = markets.values().next() {
                    let bid = m.orderbook.best_bid().unwrap_or(0.0);
                    let ask = m.orderbook.best_ask().unwrap_or(1.0);
                    spread = (ask - bid).abs();
                    mkt_price = m.last_price;
                }
                println!("[HFT] {:?} | Bal ${:.2} | {} ${:.2} | DVOL {:.1} | Mkt ${:.4} | Spread {:.4}",
                    self.state.state, self.state.simulated_balance, self.asset.to_uppercase(), binance_price, active_dvol, mkt_price, spread);
            }
        }

        if let Some(new_id) = self.market_id_rx.borrow().clone() {
            if self.state.active_market_id.is_none() {
                self.state.active_market_id = Some(new_id);
            }
        }
    }

    async fn execute_buy(&mut self, market: &Market, order: &OrderParams) {
        let (entry_price, size) = order.price_and_size();

        if self.trading_enabled {
            self.state.state = BotState::PendingBuy;
            self.state.active_market_id = Some(market.id.clone());
            self.state.pending_since = Utc::now().timestamp_millis() + (self.time_offset * 1000);
            self.state.entry_price = entry_price;
            self.state.position_size = size;
            self.state.save();

            if self.sign_and_submit(market, order, Side::Buy).await {
                self.traded_markets.insert(market.id.clone());
                println!("[PROD] Buy Order Submitted: {}@${:.4}", size, entry_price);
                let msg = format!("TRADE PLACED! {} @ ${:.4}", self.asset, entry_price);
                telegram::send_alert(&msg);
            } else {
                self.state.state = BotState::Idle;
                self.state.active_market_id = None;
                self.state.pending_since = 0;
                self.state.entry_price = 0.0;
                self.state.position_size = 0.0;
                self.state.save();
            }
        } else {
            self.state.state = BotState::InPosition;
            self.state.position_size = size;
            self.state.entry_price = entry_price;
            self.state.active_market_id = Some(market.id.clone());
            self.traded_markets.insert(market.id.clone());
            self.state.save();
            println!("[SIM] Entry at ${:.4}", entry_price);
            let msg = format!("TRADE PLACED! {} @ ${:.4}", self.asset, entry_price);
            telegram::send_alert(&msg);
        }
    }

    async fn emergency_sell(&mut self, market: &Market) {
        let order = match self.strategy.sell_order(market, self.state.position_size) {
            Some(o) => o,
            None => return,
        };
        let (sell_price, size) = order.price_and_size();

        if self.trading_enabled {
            self.state.state = BotState::PendingSell;
            self.state.pending_since = Utc::now().timestamp_millis() + (self.time_offset * 1000);
            self.state.save();
            if !self.sign_and_submit(market, &order, Side::Sell).await {
                println!("[PROD] Emergency sell failed. Reverting to InPosition.");
                self.state.state = BotState::InPosition;
                self.state.pending_since = 0;
                self.state.save();
            }
        } else {
            let pnl = (sell_price - self.state.entry_price) * size;
            self.state.simulated_balance += pnl;
            self.state.state = BotState::Idle;
            self.state.position_size = 0.0;
            self.state.active_market_id = None;
            self.state.save();
            println!("[SIM] Emergency exit at ${:.4}", sell_price);
        }
    }

    async fn sign_and_submit(&self, market: &Market, order: &OrderParams, side: Side) -> bool {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return false;
        };

        let signable = match order {
            OrderParams::Limit { price, size } => {
                let price_dec = Decimal::from_str(&format!("{:.2}", price)).unwrap_or(Decimal::ZERO);
                let size_dec = Decimal::from_str(&format!("{:.2}", size)).unwrap_or(Decimal::ZERO);
                if price_dec <= Decimal::ZERO || size_dec <= Decimal::ZERO { return false; }

                client.limit_order()
                    .token_id(&market.id)
                    .price(price_dec)
                    .size(size_dec)
                    .side(side)
                    .build().await
            }
            OrderParams::Market { amount } => {
                let amount_dec = Decimal::from_str(&format!("{:.2}", amount)).unwrap_or(Decimal::ZERO);
                if amount_dec <= Decimal::ZERO { return false; }

                let amt = if side == Side::Buy {
                    Amount::usdc(amount_dec)
                } else {
                    Amount::shares(amount_dec)
                };
                let amt = match amt {
                    Ok(a) => a,
                    Err(_) => return false,
                };

                client.market_order()
                    .token_id(&market.id)
                    .amount(amt)
                    .side(side)
                    .build().await
            }
        };

        if let Ok(order) = signable {
            if let Ok(signed_order) = client.sign(signer, order).await {
                if let Ok(resp) = client.post_order(signed_order).await {
                    println!("[PROD] Order Submitted! ID: {:?}", resp.order_id);
                    return true;
                }
            }
        }
        false
    }

    pub async fn reconcile_with_chain(&mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let usdc_fut = self.check_usdc_balance();
            let position_fut = async {
                if let Some(market_id) = self.state.active_market_id.clone() {
                    let markets = self.polymarket_rx.borrow().clone();
                    if let Some(market) = markets.get(&market_id) {
                        return self.check_on_chain_position(market).await;
                    }
                }
                None
            };

            let (usdc_result, position_result) = tokio::join!(usdc_fut, position_fut);

            if self.trading_enabled {
                if let Some(usdc_bal) = usdc_result {
                    if (self.state.simulated_balance - usdc_bal).abs() > 0.05 {
                        println!("[RECON] Syncing Capital: USDC ${:.2}", usdc_bal);
                        self.state.simulated_balance = usdc_bal;
                        self.state.save();
                    }
                }
            }

            if self.state.active_market_id.is_some() {
                if let Some(on_chain_shares) = position_result {

                        let now_ms = Utc::now().timestamp_millis() + (self.time_offset * 1000);

                        if self.state.state == BotState::PendingBuy {
                            if on_chain_shares > 0.01 {
                                println!("[RECON] Buy confirmed on-chain.");
                                self.state.state = BotState::InPosition;
                                self.state.position_size = on_chain_shares;
                                self.state.pending_since = 0;
                                self.state.save();
                            } else if now_ms - self.state.pending_since > 5000 {
                                println!("[RECON] Buy failed/timeout. Reverting to Idle.");
                                self.state.state = BotState::Idle;
                                self.state.active_market_id = None;
                                self.state.entry_price = 0.0;
                                self.state.position_size = 0.0;
                                self.state.pending_since = 0;
                                self.state.save();
                            }
                        } else if self.state.state == BotState::PendingSell {
                            if on_chain_shares < 0.01 {
                                println!("[RECON] Sell confirmed on-chain.");
                                let msg = format!("Sold. Balance: ${:.2}", self.state.simulated_balance);
                                telegram::send_alert(&msg);
                                self.state.state = BotState::Idle;
                                self.state.active_market_id = None;
                                self.state.entry_price = 0.0;
                                self.state.position_size = 0.0;
                                self.state.pending_since = 0;
                                self.state.save();
                            } else if now_ms - self.state.pending_since > 5000 {
                                println!("[RECON] Sell failed/timeout. Reverting to InPosition.");
                                self.state.state = BotState::InPosition;
                                self.state.position_size = on_chain_shares;
                                self.state.pending_since = 0;
                                self.state.save();
                            }
                        } else if (on_chain_shares - self.state.position_size).abs() > 0.01 {
                            println!("[RECON] Share drift detected. Engine: {:.2}, Chain: {:.2}", self.state.position_size, on_chain_shares);
                            self.state.position_size = on_chain_shares;
                            if on_chain_shares < 0.01 {
                                self.state.state = BotState::Idle;
                                self.state.active_market_id = None;
                                self.state.entry_price = 0.0;
                                self.state.pending_since = 0;
                            } else {
                                self.state.state = BotState::InPosition;
                            }
                            self.state.save();
                        }
                    }
                }
        }).await;
    }

    async fn check_usdc_balance(&self) -> Option<f64> {
        let provider = Provider::<Http>::try_from(self.polygon_rpc_url.as_str()).ok()?;
        let signer = self.signer_instance.as_ref()?;
        let funder: Address = Address::from(alloy_signer::Signer::address(signer).0 .0);

        let usdc_address = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174".parse::<Address>().ok()?;
        let mut call_data = [0x70, 0xa0, 0x82, 0x31, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].to_vec();
        call_data.extend_from_slice(funder.as_bytes());

        let tx = TransactionRequest::new().to(usdc_address).data(call_data).from(funder);
        provider.call(&tx.into(), None).await.ok().map(|res| {
            let bal = U256::from_big_endian(&res);
            bal.as_u128() as f64 / 1_000_000.0
        })
    }

    async fn check_on_chain_position(&self, market: &Market) -> Option<f64> {
        let provider = Provider::<Http>::try_from(self.polygon_rpc_url.as_str()).ok()?;
        let signer = self.signer_instance.as_ref()?;
        let funder: Address = Address::from(alloy_signer::Signer::address(signer).0 .0);

        let token_id = U256::from_dec_str(&market.id).ok()?;
        let ctf_address = "0x4D97d6599A46602052E175369CeBa61a5b8cae6a".parse::<Address>().ok()?;

        let mut data = vec![0x00, 0xfd, 0xd5, 0x8e];
        data.extend_from_slice(&[0u8; 12]);
        data.extend_from_slice(funder.as_bytes());
        let mut id_bytes = [0u8; 32];
        token_id.to_big_endian(&mut id_bytes);
        data.extend_from_slice(&id_bytes);

        let tx = TransactionRequest::new().to(ctf_address).data(data);
        if let Ok(res) = provider.call(&tx.into(), None).await {
            let bal = U256::from_big_endian(&res);
            return Some(bal.as_u128() as f64 / 1_000_000.0);
        }
        None
    }

    pub async fn initialize_client(&mut self, private_key: &str) -> bool {
        let signer = LocalSigner::from_str(private_key).unwrap().with_chain_id(Some(POLYGON));
        let client_builder = ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();

        match client_builder.authentication_builder(&signer).authenticate().await {
            Ok(client) => {
                self.client = Some(client);
                self.signer_instance = Some(signer);
                true
            }
            Err(e) => {
                eprintln!("[SDK] Auth failed: {}", e);
                false
            }
        }
    }
}
