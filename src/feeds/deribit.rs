use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

use super::{backoff_secs, parse_json, push_history};

pub fn spawn_deribit_dvol_ws(
    asset: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://www.deribit.com/ws/api/v2").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Deribit DVOL WS");
                    let sub = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/subscribe",
                        "id": 1,
                        "params": {
                            "channels": [format!("deribit_volatility_index.{}_usd", asset)]
                        }
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let vol = parse_json(&text).and_then(|j| {
                                    (j["method"] == "subscription").then(|| {
                                        let data = &j["params"]["data"];
                                        let v = data["volatility"].as_f64()?;
                                        let ts = data["timestamp"].as_i64().unwrap_or_else(crate::common::time::now_ms);
                                        Some((v, ts))
                                    })?
                                });
                                if let Some((val, ts)) = vol {
                                    let _ = tx.send((val, ts));
                                    push_history(&history, val, ts, window_ms);
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Deribit WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}
