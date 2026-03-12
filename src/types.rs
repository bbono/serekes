use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
}

impl Market {
    pub fn new(slug: String, up_token_id: String, down_token_id: String, started_at_ms: i64, expires_at_ms: i64, strike_price_binance: f64) -> Self {
        Self {
            slug,
            started_at_ms,
            expires_at_ms,
            strike_price_binance,
            strike_price: 0.0,
            up: TokenSide::new(up_token_id),
            down: TokenSide::new(down_token_id),
        }
    }
}

// ---------------------------------------------------------------------------
// Order types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum Side {
    Buy,
    Sell,
}

/// Describes how to place an order. The engine signs and submits it
/// to the Polymarket CLOB via the SDK.
///
/// The strategy chooses the order type; the engine handles signing,
/// Decimal conversion, and error recovery.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum OrderParams {
    /// Limit order — placed via `client.limit_order()`.
    /// Sits on the book at the exact price until filled or cancelled.
    ///
    /// `price`: limit price in Polymarket token units (0.01–0.99 range).
    ///   For buys, this is the max you'll pay per share.
    ///   For sells, this is the min you'll accept per share.
    ///
    /// `size`: number of shares (must be >= min order size on Polymarket,
    ///   currently 5.0 shares). Rounded to 2 decimal places by the engine.
    Limit {
        price: f64,
        size: f64,
        order_type: MarketOrderType,
    },

    /// Market order — placed via `client.market_order()`.
    /// Fills immediately against resting orders at best available price.
    ///
    /// `amount`: for buy orders this is the USDC amount to spend.
    ///   For sell orders this is the number of shares to sell.
    ///
    /// `order_type`: FAK (fill-and-kill, default) or FOK (fill-or-kill).
    Market {
        amount: f64,
        order_type: MarketOrderType,
    },
}

/// Fill type for market orders.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum MarketOrderType {
    /// Fill-and-kill: fill as much as possible, cancel the rest.
    FAK,
    /// Fill-or-kill: fill entirely or cancel the whole order.
    FOK,
}

impl OrderParams {
    /// Returns (price, size) for the engine's state tracking.
    ///
    /// For limit orders: returns the exact price and size.
    /// For market orders: returns (amount, amount) as a placeholder —
    /// the engine reconciles the actual fill via on-chain position check.
    pub fn price_and_size(&self) -> (f64, f64) {
        match self {
            OrderParams::Limit { price, size, .. } => (*price, *size),
            OrderParams::Market { amount, .. } => (*amount, *amount),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine tick snapshot
// ---------------------------------------------------------------------------

/// Snapshot of all market state passed to strategy on every engine tick (1ms loop).
///
/// The engine builds this from its WebSocket data feeds, shared market state,
/// and risk monitors, so the strategy never touches raw watch receivers or
/// infrastructure. All fields are copied values — no references, no async, pure data.
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
}

// ---------------------------------------------------------------------------
// Trade result
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
pub struct Trade {
    pub side: Side,
    pub direction: TokenDirection,
    pub price: f64,
    pub size: f64,
    pub order_params: OrderParams,
    pub order_id: String,
    pub success: bool,
    pub error_msg: Option<String>,
}
