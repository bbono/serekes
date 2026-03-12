use crate::types::{Market, TokenDirection};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
/// Snapshot of market state passed to strategy on every engine tick (1ms loop).
///
/// The engine builds this from its WebSocket data feeds and risk monitors,
/// so the strategy never touches raw watch receivers or infrastructure.
/// All fields are copied values — no references, no async, pure data.
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

    /// Recent binance price history: (price, timestamp_ms), oldest first.
    /// Lock only when needed — avoid holding across await points.
    pub binance_history: Arc<Mutex<VecDeque<(f64, i64)>>>,

    /// Recent chainlink price history: (price, timestamp_ms), oldest first.
    /// Lock only when needed — avoid holding across await points.
    pub chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>>,
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

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, order)) to buy.
    fn create_entry_order(
        &self,
        ctx: &TickContext,
        market: &Market,
    ) -> Option<(TokenDirection, OrderParams)>;

    /// Called each tick while holding a position.
    /// Return Some(OrderParams) to sell, None to keep holding.
    #[allow(dead_code)]
    fn create_exit_order(
        &self,
        _ctx: &TickContext,
        _market: &Market,
        _position_size: f64,
    ) -> Option<OrderParams> {
        None
    }

    /// Whether this strategy manages its own exit logic.
    /// If false, the engine considers itself done immediately after entry.
    #[allow(dead_code)]
    fn manages_exit(&self) -> bool {
        false
    }
}
