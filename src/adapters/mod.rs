pub mod binance;
pub mod chainlink;
pub mod clock;
pub mod coinbase;
pub mod notion;
pub mod polymarket_discovery;
pub mod polymarket_order;
pub mod polymarket_price;
pub mod resolver;
pub mod telegram;

use std::collections::VecDeque;
use std::sync::Mutex;

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

fn lookup_history_impl(
    history: &Mutex<VecDeque<(f64, i64)>>,
    target_ms: i64,
    exact: bool,
) -> f64 {
    let hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.iter()
        .rev()
        .find(|(_, ts)| {
            if exact {
                *ts == target_ms
            } else {
                *ts <= target_ms
            }
        })
        .map(|(price, _)| *price)
        .unwrap_or(0.0)
}
