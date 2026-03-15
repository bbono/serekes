use crate::ports::{ClockPort, PriceFeedPort};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, warn};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use super::{backoff_secs, parse_json};

/// Coinbase ticker WebSocket adapter implementing PriceFeedPort.
pub struct CoinbaseAdapter {
    rx: watch::Receiver<(f64, i64)>,
}

impl CoinbaseAdapter {
    pub fn spawn(asset: &str, clock: Arc<dyn ClockPort>) -> Self {
        let (tx, rx) = watch::channel((0.0f64, 0i64));
        let product = format!("{}-USD", asset.to_uppercase());

        tokio::spawn(async move {
            let mut attempts = 0u32;
            loop {
                match connect_async("wss://ws-feed.exchange.coinbase.com").await {
                    Ok((mut ws_stream, _)) => {
                        attempts = 0;
                        debug!("Connected to Coinbase WS");
                        let sub = serde_json::json!({
                            "type": "subscribe",
                            "product_ids": [product],
                            "channels": ["ticker"]
                        });
                        let _ = ws_stream.send(Message::Text(sub.to_string().into())).await;
                        let (mut write, mut read) = ws_stream.split();
                        while let Some(Ok(msg)) = read.next().await {
                            match msg {
                                Message::Text(text) => {
                                    let price = parse_json(&text)
                                        .and_then(|j| j["price"].as_str()?.parse::<f64>().ok());
                                    if let Some(price) = price {
                                        let _ = tx.send((price, clock.now_ms()));
                                    }
                                }
                                Message::Ping(data) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        warn!("Coinbase WS disconnected. Reconnecting...");
                    }
                    Err(e) => {
                        let delay = backoff_secs(attempts);
                        error!("Coinbase WS error: {}. Reconnecting in {}s...", e, delay);
                        sleep(Duration::from_secs(delay)).await;
                        attempts += 1;
                    }
                }
            }
        });

        Self { rx }
    }
}

impl PriceFeedPort for CoinbaseAdapter {
    fn latest(&self) -> (f64, i64) {
        *self.rx.borrow()
    }

    fn is_ready(&self) -> bool {
        self.rx.borrow().0 > 0.0
    }
}
