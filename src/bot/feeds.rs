use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

fn backoff_secs(attempt: u32) -> u64 {
    (5u64 << attempt.min(4)).min(60)
}

fn push_history(history: &Mutex<VecDeque<(f64, i64)>>, price: f64, ts: i64, window_ms: i64) {
    let mut hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.push_back((price, ts));
    let cutoff = ts - window_ms;
    while hist.front().is_some_and(|(_, t)| *t < cutoff) {
        hist.pop_front();
    }
}

fn parse_json(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

pub fn spawn_binance_ws(
    url: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse(&url).unwrap()).await {
                Ok((ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Binance WS");
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let price = parse_json(&text).and_then(|j| {
                                    let ts = j["T"].as_i64().unwrap_or_else(crate::common::time::now_ms);
                                    j["p"].as_str()?.parse::<f64>().ok().map(|p| (p, ts))
                                });
                                if let Some((price, ts)) = price {
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
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Binance WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

pub fn spawn_coinbase_ws(product: String, tx: watch::Sender<(f64, i64)>) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-feed.exchange.coinbase.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Coinbase WS");
                    let sub = serde_json::json!({
                        "type": "subscribe",
                        "product_ids": [product],
                        "channels": ["ticker"]
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let price = parse_json(&text)
                                    .and_then(|j| j["price"].as_str()?.parse::<f64>().ok());
                                if let Some(price) = price {
                                    let _ = tx.send((price, crate::common::time::now_ms()));
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
                    error!("Coinbase WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

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

pub fn spawn_chainlink_ws(
    asset: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-live-data.polymarket.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Chainlink WS");
                    let sub = serde_json::json!({
                        "action": "subscribe",
                        "subscriptions": [{
                            "topic": "crypto_prices_chainlink",
                            "type": "update",
                            "filters": format!("{{\"symbol\":\"{}/usd\"}}", asset)
                        }]
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
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
