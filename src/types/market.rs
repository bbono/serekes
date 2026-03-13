/// One side (Up or Down) of a binary market.
#[derive(Debug, Clone)]
pub struct TokenSide {
    pub token_id: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub last_updated: i64,
}

impl TokenSide {
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            best_bid: 0.0,
            best_ask: 0.0,
            last_updated: 0,
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
