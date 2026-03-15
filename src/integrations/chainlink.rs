use futures_util::{SinkExt, StreamExt};
use log::{debug, error, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use super::{backoff_secs, parse_json, push_history};

pub fn spawn_chainlink_ws(
    asset: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async("wss://ws-live-data.polymarket.com").await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    debug!("Connected to Chainlink WS");
                    let sub = serde_json::json!({
                        "action": "subscribe",
                        "subscriptions": [{
                            "topic": "crypto_prices_chainlink",
                            "type": "update",
                            "filters": format!("{{\"symbol\":\"{}/usd\"}}", asset)
                        }]
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string().into())).await;
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let tick = parse_json(&text).and_then(|j| {
                                    let p = j["payload"]["value"].as_f64()?;
                                    let ts = j["payload"]["timestamp"].as_i64()?;
                                    Some((p, ts))
                                });
                                if let Some((price, ts)) = tick {
                                    let _ = tx.send((price, ts));
                                    push_history(&history, price, ts, window_ms);
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    warn!("Chainlink WS disconnected. Reconnecting...");
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Chainlink WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}
