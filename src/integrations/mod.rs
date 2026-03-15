mod binance;
mod chainlink;
mod coinbase;
pub mod notion;
pub mod polymarket;
pub mod telegram;

pub use binance::spawn_binance_ws;
pub use chainlink::spawn_chainlink_ws;
pub use coinbase::spawn_coinbase_ws;
pub use polymarket::{connect_poly_price_ws, discover_market, resolve_strike_prices};

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
