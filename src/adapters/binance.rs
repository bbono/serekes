use crate::ports::{ClockPort, PriceFeedPort};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use super::{backoff_secs, parse_json, push_history, lookup_history_impl};

/// Binance aggTrade WebSocket adapter implementing PriceFeedPort.
pub struct BinanceAdapter {
    rx: watch::Receiver<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
}

impl BinanceAdapter {
    pub fn spawn(asset: &str, clock: Arc<dyn ClockPort>, window_ms: i64) -> Self {
        let (tx, rx) = watch::channel((0.0f64, 0i64));
        let history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));

        let url = format!(
            "wss://stream.binance.com:9443/ws/{}usdt@aggTrade",
            asset
        );
        let hist = history.clone();
        tokio::spawn(async move {
            let mut attempts = 0u32;
            loop {
                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        attempts = 0;
                        debug!("Connected to Binance WS");
                        let (mut write, mut read) = ws_stream.split();
                        while let Some(Ok(msg)) = read.next().await {
                            match msg {
                                Message::Text(text) => {
                                    let price = parse_json(&text).and_then(|j| {
                                        let ts = j["T"].as_i64().unwrap_or_else(|| clock.now_ms());
                                        j["p"].as_str()?.parse::<f64>().ok().map(|p| (p, ts))
                                    });
                                    if let Some((price, ts)) = price {
                                        let _ = tx.send((price, ts));
                                        push_history(&hist, price, ts, window_ms);
                                    }
                                }
                                Message::Ping(data) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                        warn!("Binance WS disconnected. Reconnecting...");
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

        Self { rx, history }
    }
}

impl PriceFeedPort for BinanceAdapter {
    fn latest(&self) -> (f64, i64) {
        *self.rx.borrow()
    }

    fn is_ready(&self) -> bool {
        self.rx.borrow().0 > 0.0
    }

    fn lookup_history(&self, target_ms: i64, exact: bool) -> f64 {
        lookup_history_impl(&self.history, target_ms, exact)
    }
}
