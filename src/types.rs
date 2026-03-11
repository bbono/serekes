use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<(String, String)>,
    pub asks: Vec<(String, String)>,
}

#[allow(dead_code)]
impl Default for OrderBook {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().and_then(|(p, _)| p.parse::<f64>().ok())
    }

    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().and_then(|(p, _)| p.parse::<f64>().ok())
    }

    pub fn update(&mut self, bids: Option<Vec<(String, String)>>, asks: Option<Vec<(String, String)>>) {
        if let Some(new_bids) = bids {
            for (price, size) in new_bids {
                if size == "0" {
                    self.bids.retain(|(p, _)| p != &price);
                } else if let Some(pos) = self.bids.iter().position(|(p, _)| p == &price) {
                    self.bids[pos] = (price, size);
                } else {
                    self.bids.push((price, size));
                }
            }
            self.bids.sort_by(|(p1, _), (p2, _)| {
                let p1_f = p1.parse::<f64>().unwrap_or(0.0);
                let p2_f = p2.parse::<f64>().unwrap_or(0.0);
                p2_f.partial_cmp(&p1_f).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        if let Some(new_asks) = asks {
            for (price, size) in new_asks {
                if size == "0" {
                    self.asks.retain(|(p, _)| p != &price);
                } else if let Some(pos) = self.asks.iter().position(|(p, _)| p == &price) {
                    self.asks[pos] = (price, size);
                } else {
                    self.asks.push((price, size));
                }
            }
            self.asks.sort_by(|(p1, _), (p2, _)| {
                let p1_f = p1.parse::<f64>().unwrap_or(0.0);
                let p2_f = p2.parse::<f64>().unwrap_or(0.0);
                p1_f.partial_cmp(&p2_f).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

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
    pub orderbook: OrderBook,
    pub orderbook_mid_price: f64,
    pub last_trade_price: f64,
    pub best_bid: f64,
    pub best_ask: f64,
}

impl TokenSide {
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            orderbook: OrderBook::new(),
            orderbook_mid_price: 0.0,
            last_trade_price: 0.0,
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
