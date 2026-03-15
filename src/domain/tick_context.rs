use std::sync::Arc;

use super::{Market, Trade};

/// Snapshot of all market state passed to strategy on every engine tick (1ms loop).
///
/// The engine builds this from its port adapters and internal state,
/// so the strategy never touches infrastructure. All fields are copied
/// values — no references, no async, pure data.
#[derive(Debug)]
#[allow(dead_code)]
pub struct TickContext {
    /// Current BTC (or configured asset) spot price from Binance aggTrade stream.
    pub binance_price: f64,
    /// Unix ms when binance_price was last updated.
    pub binance_ts: i64,

    /// Current spot price from Coinbase ticker stream.
    pub coinbase_price: f64,
    /// Unix ms when coinbase_price was last updated.
    pub coinbase_ts: i64,

    /// Current spot price from Chainlink oracle via Polymarket's live-data WS.
    pub chainlink_price: f64,
    /// Unix ms when chainlink_price was last updated.
    pub chainlink_ts: i64,

    /// Current unix ms, adjusted for Polymarket server time offset.
    pub polymarket_now_ms: i64,

    /// Polymarket binary market snapshot (static metadata + live bid/ask from poly WS).
    /// None if no market has been discovered yet.
    /// Wrapped in Arc to avoid cloning the full Market struct every tick.
    pub market: Option<Arc<Market>>,

    /// All trades placed by the engine during this market, oldest first.
    pub trades: Vec<Trade>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TickResult {
    pub traded: bool,
    pub trades: Vec<Trade>,
    pub timestamp_ms: i64,
    pub completed: bool,
}
