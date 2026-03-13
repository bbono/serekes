use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType, Side};
use polymarket_client_sdk::types::Decimal;

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum TokenDirection {
    Up,
    Down,
}

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
    /// Market orders have no known price until fill, so price is 0.0.
    pub fn price_and_size(&self) -> (f64, f64) {
        match self {
            OrderIntent::Limit { price, size, .. } => {
                (decimal_to_f64(*price), decimal_to_f64(*size))
            }
            OrderIntent::Market { amount, .. } => {
                (0.0, decimal_to_f64(*amount))
            }
        }
    }

    /// Returns the USDC cost of the order.
    /// Market buy: amount (USDC spent). Limit buy: price * size.
    /// Sell orders return 0.0 (no USDC outflow).
    pub fn cost(&self) -> f64 {
        if self.side() == Side::Sell {
            return 0.0;
        }
        match self {
            OrderIntent::Market { amount, .. } => decimal_to_f64(*amount),
            OrderIntent::Limit { price, size, .. } => {
                decimal_to_f64(*price) * decimal_to_f64(*size)
            }
        }
    }
}

fn decimal_to_f64(d: Decimal) -> f64 {
    d.try_into().unwrap_or(0.0)
}

#[derive(Debug, Clone)]
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
