use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType, Side};
use polymarket_client_sdk::types::Decimal;

// ---------------------------------------------------------------------------
// Engine types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EngineState {
    Idle,
    InPosition,
}

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, intent)) to enter.
    fn create_entry_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderIntent)>;
}

// ---------------------------------------------------------------------------
// Market types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum TokenDirection {
    Up,
    Down,
}

/// One side (Up or Down) of a binary market.
#[derive(Debug, Clone)]
pub struct TokenSide {
    pub token_id: String,
    pub best_bid: f64,
    pub best_ask: f64,
}

impl TokenSide {
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            best_bid: 0.0,
            best_ask: 0.0,
        }
    }
}

/// A single Polymarket binary market containing both Up and Down sides.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Market {
    pub slug: String,
    pub started_at_ms: i64,
    pub expires_at_ms: i64,
    pub strike_price_binance: f64,
    pub strike_price: f64,
    pub up: TokenSide,
    pub down: TokenSide,
    /// Minimum tick size for price precision (from Gamma API).
    pub tick_size: f64,
    /// Minimum order size in USDC (from Gamma API).
    pub min_order_size: f64,
}

impl Market {
    pub fn new(slug: String, up_token_id: String, down_token_id: String, started_at_ms: i64, expires_at_ms: i64, tick_size: f64, min_order_size: f64) -> Self {
        Self {
            slug,
            started_at_ms,
            expires_at_ms,
            strike_price_binance: 0.0,
            strike_price: 0.0,
            up: TokenSide::new(up_token_id),
            down: TokenSide::new(down_token_id),
            tick_size,
            min_order_size,
        }
    }
}

// ---------------------------------------------------------------------------
// Order types (Side, OrderType, Decimal come from polymarket_client_sdk)
// ---------------------------------------------------------------------------

/// Everything the engine needs to submit an order to the CLOB.
/// The strategy fully configures this; the engine only adds `token_id`
/// (from direction routing) and calls `.build().await`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum OrderIntent {
    Limit {
        side: Side,
        price: Decimal,
        size: Decimal,
        order_type: OrderType,
    },
    Market {
        side: Side,
        amount: Decimal,
        order_type: OrderType,
    },
}

#[allow(dead_code)]
impl OrderIntent {
    pub fn side(&self) -> Side {
        match self {
            OrderIntent::Limit { side, .. } | OrderIntent::Market { side, .. } => *side,
        }
    }

    /// Returns (price, size) as f64 for paper-trade simulation and logging.
    pub fn price_and_size(&self) -> (f64, f64) {
        match self {
            OrderIntent::Limit { price, size, .. } => {
                (decimal_to_f64(*price), decimal_to_f64(*size))
            }
            OrderIntent::Market { amount, .. } => {
                let a = decimal_to_f64(*amount);
                (a, a)
            }
        }
    }
}

fn decimal_to_f64(d: Decimal) -> f64 {
    d.try_into().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Engine tick snapshot
// ---------------------------------------------------------------------------

/// Snapshot of all market state passed to strategy on every engine tick (1ms loop).
///
/// The engine builds this from its WebSocket data feeds, shared market state,
/// and risk monitors, so the strategy never touches raw watch receivers or
/// infrastructure. All fields are copied values — no references, no async, pure data.
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

    /// Deribit implied volatility index (DVOL).
    pub dvol: f64,
    /// Unix ms when dvol was last updated.
    pub dvol_ts: i64,

    /// Current unix ms, adjusted for Polymarket server time offset.
    pub now_ms: i64,

    /// Polymarket binary market snapshot (static metadata + live bid/ask from poly WS).
    /// None if no market has been discovered yet.
    pub market: Option<Market>,

    /// Recent binance price history: (price, timestamp_ms), oldest first.
    /// Lock only when needed — avoid holding across await points.
    pub binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,

    /// Recent chainlink price history: (price, timestamp_ms), oldest first.
    /// Lock only when needed — avoid holding across await points.
    pub chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,

    /// Recent DVOL history: (volatility, timestamp_ms), oldest first.
    pub dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
}

// ---------------------------------------------------------------------------
// Trade result
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
pub struct Trade {
    pub direction: TokenDirection,
    pub intent: OrderIntent,
    pub price: f64,
    pub size: f64,
    pub order_id: String,
    pub order_status: OrderStatusType,
    pub timestamp_ms: i64,
}
