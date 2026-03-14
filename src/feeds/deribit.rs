use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
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

                    // Enable server heartbeats (every 30s) to keep connection alive
                    let heartbeat_req = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/set_heartbeat",
                        "id": 2,
                        "params": { "interval": 30 }
                    });
                    let _ = ws_stream.send(Message::Text(heartbeat_req.to_string())).await;

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
                                if let Some(j) = parse_json(&text) {
                                    // Respond to heartbeat test_request
                                    if j["params"]["type"] == "test_request" {
                                        let test_resp = serde_json::json!({
                                            "jsonrpc": "2.0",
                                            "method": "public/test",
                                            "id": 3,
                                            "params": {}
                                        });
                                        let _ = write.send(Message::Text(test_resp.to_string())).await;
                                        continue;
                                    }
                                    // Process subscription data
                                    if j["method"] == "subscription" {
                                        let data = &j["params"]["data"];
                                        if let Some(v) = data["volatility"].as_f64() {
                                            let ts = data["timestamp"].as_i64().unwrap_or_else(crate::common::time::now_ms);
                                            let _ = tx.send((v, ts));
                                            push_history(&history, v, ts, window_ms);
                                        }
                                    }
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    warn!("Deribit DVOL WS disconnected. Reconnecting...");
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
