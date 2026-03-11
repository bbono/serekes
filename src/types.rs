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
    pub last_trade_price: f64,
}

impl TokenSide {
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            last_trade_price: 0.0,
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

#[allow(dead_code)]
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

    /// Get the token side matching a token ID, if any.
    pub fn side_by_token(&mut self, token_id: &str) -> Option<&mut TokenSide> {
        if self.up.token_id == token_id {
            Some(&mut self.up)
        } else if self.down.token_id == token_id {
            Some(&mut self.down)
        } else {
            None
        }
    }
}
