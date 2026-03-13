use polymarket_client_sdk::clob::types::OrderStatusType;

use super::{OrderIntent, TokenDirection};

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
