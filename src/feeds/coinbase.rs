use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

use super::{backoff_secs, parse_json};

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
