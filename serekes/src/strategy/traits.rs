use crate::types::Market;

/// Snapshot of market state passed to strategy on every engine tick (1ms loop).
///
/// The engine builds this from its WebSocket data feeds and risk monitors,
/// so the strategy never touches raw watch receivers or infrastructure.
/// All fields are copied values — no references, no async, pure data.
#[allow(dead_code)]
pub struct TickContext {
    /// Current BTC (or configured asset) spot price from Binance aggTrade stream.
    /// This is the primary oracle — all gap/strike calculations compare against it.
    /// Updated every ~100ms from wss://stream.binance.com.
    pub binance_price: f64,

    /// Current spot price from Coinbase ticker stream.
    /// Used as a secondary oracle for cross-exchange validation.
    /// The engine already uses this for the divergence exit guard,
    /// but strategies can use it for additional cross-exchange signals.
    /// 0.0 if the Coinbase feed hasn't connected yet.
    pub coinbase_price: f64,

    /// Deribit implied volatility index (DVOL) for the configured asset.
    /// Ranges roughly 20–120 under normal conditions.
    /// Strategies can use this to adjust aggressiveness based on market regime
    /// (e.g. widen entry thresholds during high vol, tighten during low vol).
    /// 0.0 if the Deribit feed hasn't connected yet; the engine falls back to
    /// ATR-based vol internally, but strategies see the raw value here.
    pub dvol: f64,

    /// Current unix timestamp in milliseconds, already adjusted for the
    /// Polymarket server time offset. Use this for all time comparisons
    /// (e.g. `market.expiration - (ctx.now_ms / 1000)` gives seconds to expiry).
    pub now_ms: i64,
}

/// Returned by `check_exit` to tell the engine why to close a position.
///
/// Engine-level exits that fire before `check_exit` is ever called:
/// - Expiry settlement (time_to_expiry <= 0): engine computes Up/Down payout automatically
/// - Exchange divergence: Coinbase vs Binance spread exceeds `divergence_exit_pct`
/// - Velocity lockout: blocks new entries (not exits) during flash crashes
pub enum ExitReason {
    /// Engine calls `sell_order()` to get order params, then signs and submits.
    /// The string is logged as the exit reason (e.g. "GAP COLLAPSE").
    Emergency(String),
}

/// Describes how to place an order. The engine signs and submits it
/// to the Polymarket CLOB via the SDK.
///
/// The strategy chooses the order type; the engine handles signing,
/// Decimal conversion, and error recovery.
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
    Limit { price: f64, size: f64 },

    /// Market order — placed via `client.market_order()`.
    /// Fills immediately against resting orders at best available price.
    ///
    /// `amount`: for buy orders this is the USDC amount to spend.
    ///   For sell orders this is the number of shares to sell.
    ///   The engine passes it to `Amount::usdc()` or `Amount::shares()`
    ///   depending on the side.
    Market { amount: f64 },
}

impl OrderParams {
    /// Returns (price, size) for the engine's state tracking.
    ///
    /// For limit orders: returns the exact price and size.
    /// For market orders: returns (amount, amount) as a placeholder —
    /// the engine reconciles the actual fill via on-chain position check.
    pub fn price_and_size(&self) -> (f64, f64) {
        match self {
            OrderParams::Limit { price, size } => (*price, *size),
            OrderParams::Market { amount } => (*amount, *amount),
        }
    }
}

/// Defines the trading logic that the engine delegates to.
///
/// The engine handles everything common:
/// - State machine (Idle -> PendingBuy -> InPosition -> PendingSell -> Idle)
/// - Order signing and submission to Polymarket CLOB
/// - On-chain reconciliation (USDC balance, CTF position shares)
/// - Killswitch (Binance vs Coinbase price de-peg)
/// - Exchange divergence exit
/// - Velocity lockout (flash crash detection)
/// - Price extremes filter (rejects 0.00 and 1.00 tokens)
/// - Spread filter (rejects wide orderbooks)
/// - Expiry settlement (Up/Down binary payout at market expiration)
/// - Telemetry logging and Telegram alerts
///
/// A `Strategy` impl only decides *when* to trade, *what order type* to use,
/// and *at what size/price*.
///
/// To create a new strategy: implement this trait on a struct and pass it
/// to `StrategyEngine::new()` in main.rs. Only `check_entry` is required.
pub trait Strategy {
    /// Called once per tick for every market that passes engine-level guards:
    /// - Market not already traded in this session
    /// - Token price between 0.01 and 0.99 (not at extremes)
    /// - Orderbook spread within `max_spread` config threshold
    /// - Velocity monitor not in lockout (no flash crash in progress)
    ///
    /// `ctx`: current tick data (prices, volatility, timestamp).
    /// `market`: the Polymarket binary market being evaluated — contains
    ///   orderbook, expiration timestamp, strike price, and Up/Down direction.
    /// `balance`: current USDC balance (real wallet in live mode, simulated in backtest).
    ///
    /// Return `Some(OrderParams)` to place a buy order, or `None` to skip.
    /// The engine will execute at most one entry per tick (breaks after first match).
    fn check_entry(&self, ctx: &TickContext, market: &Market, balance: f64) -> Option<OrderParams>;

    /// Called once per tick while the bot holds a position (`InPosition` state),
    /// after these engine-level exit checks have already passed:
    /// - Expiry settlement (auto-settles if time_to_expiry <= 0)
    /// - Exchange divergence exit (Coinbase vs Binance spread too wide)
    ///
    /// `ctx`: current tick data. `market`: the market we're positioned in.
    ///
    /// Return `Some(ExitReason::Emergency(reason))` to trigger an exit.
    /// The engine will then call `sell_order()` to get the order params.
    /// Return `None` to keep holding.
    ///
    /// Default: `None` (hold until expiry). Buy-only strategies can skip this.
    fn check_exit(&self, _ctx: &TickContext, _market: &Market) -> Option<ExitReason> { None }

    /// Build sell order params for an emergency exit.
    /// Called by the engine immediately after `check_exit` returns `Emergency`.
    ///
    /// `market`: the market we're exiting — use its orderbook for pricing.
    /// `position_size`: current number of shares held (from engine state).
    ///
    /// Return `Some(OrderParams)` to place the sell, or `None` to skip
    /// (e.g. position too small to meet exchange minimums).
    ///
    /// Default: `None` (no sell). Buy-only strategies can skip this.
    fn sell_order(&self, _market: &Market, _position_size: f64) -> Option<OrderParams> { None }
}
