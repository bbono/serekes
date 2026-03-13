use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

use super::{backoff_secs, parse_json, push_history};

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
