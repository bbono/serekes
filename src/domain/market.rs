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

    /// Milliseconds remaining until this market expires.
    pub fn time_to_expire_ms(&self, now_ms: i64) -> i64 {
        self.expires_at_ms.saturating_sub(now_ms)
    }

    /// Milliseconds elapsed since this market started.
    #[allow(dead_code)]
    pub fn time_from_started_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.started_at_ms)
    }

    /// Compute the time bucket start in ms for a given interval.
    pub fn bucket_start_ms(now_ms: i64, interval_minutes: u32) -> i64 {
        let interval_ms = (interval_minutes as i64) * 60_000;
        (now_ms / interval_ms) * interval_ms
    }

    /// Build a market slug from asset, interval, and bucket start timestamp.
    pub fn slug_for(asset: &str, interval_minutes: u32, bucket_start_ms: i64) -> String {
        let kline_interval = if interval_minutes >= 60 {
            format!("{}h", interval_minutes / 60)
        } else {
            format!("{}m", interval_minutes)
        };
        let ts_secs = bucket_start_ms / 1000;
        format!("{}-updown-{}-{}", asset, kline_interval, ts_secs)
    }

    /// Return candidate slugs to try for market discovery (current bucket, then next).
    pub fn candidate_slugs(asset: &str, interval_minutes: u32, now_ms: i64) -> Vec<String> {
        let interval_ms = (interval_minutes as i64) * 60_000;
        let bucket = Self::bucket_start_ms(now_ms, interval_minutes);
        vec![
            Self::slug_for(asset, interval_minutes, bucket),
            Self::slug_for(asset, interval_minutes, bucket + interval_ms),
        ]
    }

    /// Compute PnL from a market resolution outcome.
    pub fn compute_pnl(outcome: &str, shares_up: f64, shares_down: f64, cost: f64) -> f64 {
        match outcome.to_lowercase().as_str() {
            "up" => shares_up - cost,
            "down" => shares_down - cost,
            _ => -cost,
        }
    }
}
