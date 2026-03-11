use crate::types::Market;
use super::traits::{Strategy, TickContext, ExitReason, OrderParams};

pub struct GapArbStrategy {
    pub gap_threshold_pct: f64,
    pub min_time_to_expiry: i64,
    pub max_time_to_expiry: i64,
    pub gap_collapse_pct: f64,
    pub gap_collapse_time: i64,
}

impl Default for GapArbStrategy {
    fn default() -> Self {
        Self {
            gap_threshold_pct: 0.005,
            min_time_to_expiry: 4,
            max_time_to_expiry: 20,
            gap_collapse_pct: 0.0006,
            gap_collapse_time: 10,
        }
    }
}

impl Strategy for GapArbStrategy {
    fn check_entry(&self, ctx: &TickContext, market: &Market, balance: f64) -> Option<OrderParams> {
        let time_to_expiry = market.expiration - (ctx.now_ms / 1000);
        if time_to_expiry <= self.min_time_to_expiry || time_to_expiry > self.max_time_to_expiry {
            return None;
        }

        let gap = (ctx.binance_price - market.strike_price).abs();
        if gap <= (ctx.binance_price * self.gap_threshold_pct) {
            return None;
        }

        let price = market.last_price;
        let size = (balance / price * 100.0).floor() / 100.0;
        if size < 5.2 { return None; }

        Some(OrderParams::Limit { price, size })
    }

    fn check_exit(&self, ctx: &TickContext, market: &Market) -> Option<ExitReason> {
        let time_to_expiry = market.expiration - (ctx.now_ms / 1000);
        let gap = (ctx.binance_price - market.strike_price).abs();

        let gap_collapse = gap < (ctx.binance_price * self.gap_collapse_pct) && time_to_expiry <= self.gap_collapse_time;

        if gap_collapse {
            return Some(ExitReason::Emergency("GAP COLLAPSE".to_string()));
        }

        None
    }

    fn sell_order(&self, market: &Market, position_size: f64) -> Option<OrderParams> {
        let size = (position_size * 100.0).floor() / 100.0;
        if size < 5.0 { return None; }

        let price = (market.orderbook.best_bid().unwrap_or(market.last_price) - 0.15).max(0.01);
        Some(OrderParams::Limit { price, size })
    }
}
