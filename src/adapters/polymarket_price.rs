use crate::domain::Market;
use crate::ports::MarketPricePort;
use futures_util::StreamExt;
use log::{debug, error};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::types::U256;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

/// Polymarket per-market bid/ask WebSocket adapter implementing MarketPricePort.
pub struct PolymarketPriceAdapter {
    shared_market: Arc<Mutex<Option<Arc<Market>>>>,
    current_handle: Mutex<Option<JoinHandle<()>>>,
}

impl PolymarketPriceAdapter {
    pub fn new() -> Self {
        Self {
            shared_market: Arc::new(Mutex::new(None)),
            current_handle: Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl MarketPricePort for PolymarketPriceAdapter {
    async fn connect(&self, market: &Market) {
        let token_ids: Vec<U256> = vec![
            U256::from_str(&market.up.token_id).expect("invalid up token_id"),
            U256::from_str(&market.down.token_id).expect("invalid down token_id"),
        ];
        debug!(
            "Connecting to Polymarket Price WS for market {}...",
            market.slug
        );
        *self.shared_market.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Arc::new(market.clone()));
        let connected = Arc::new(tokio::sync::Notify::new());
        let handle =
            spawn_poly_price_ws(self.shared_market.clone(), token_ids, connected.clone());
        connected.notified().await;
        *self.current_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    fn current_market(&self) -> Option<Arc<Market>> {
        self.shared_market
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn disconnect(&self) {
        if let Some(handle) = self
            .current_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

fn spawn_poly_price_ws(
    shared: Arc<Mutex<Option<Arc<Market>>>>,
    token_ids: Vec<U256>,
    connected: Arc<tokio::sync::Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let ws_client = PolyWsClient::default();
            let stream = match ws_client.subscribe_prices(token_ids.clone()) {
                Ok(s) => s,
                Err(e) => {
                    error!("Polymarket Price WS subscribe error: {}. Retrying...", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            debug!("Connected to Polymarket Price WS");
            connected.notify_one();
            let mut stream = Box::pin(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(price_change) => {
                        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(current) = guard.as_ref() {
                            let mut updated = (**current).clone();
                            let ts_ms = price_change.timestamp * 1000;
                            let mut changed = false;
                            for entry in &price_change.price_changes {
                                let asset_str = entry.asset_id.to_string();
                                let is_up = asset_str == updated.up.token_id;
                                let is_down = asset_str == updated.down.token_id;
                                if !is_up && !is_down {
                                    continue;
                                }

                                let side =
                                    if is_up { &mut updated.up } else { &mut updated.down };

                                if let Some(bid) = &entry.best_bid {
                                    let v: f64 = bid.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_bid = v;
                                        changed = true;
                                    }
                                }
                                if let Some(ask) = &entry.best_ask {
                                    let v: f64 = ask.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_ask = v;
                                        changed = true;
                                    }
                                }
                                if changed {
                                    side.last_updated = ts_ms;
                                }
                            }
                            if changed {
                                *guard = Some(Arc::new(updated));
                            }
                        }
                    }
                    Err(e) => {
                        error!("Polymarket Price WS error: {}. Reconnecting...", e);
                        break;
                    }
                }
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
}
