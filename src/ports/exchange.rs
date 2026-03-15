use crate::domain::OrderIntent;
use polymarket_client_sdk::clob::types::OrderStatusType;
use polymarket_client_sdk::types::Decimal;

/// Result of a submitted order.
pub struct OrderResult {
    pub order_id: String,
    pub status: OrderStatusType,
    pub making_amount: Decimal,
    pub taking_amount: Decimal,
}

/// Port for signing and submitting orders to an exchange.
#[async_trait::async_trait]
pub trait ExchangePort: Send + Sync {
    /// Sign and submit an order. Returns the exchange response.
    async fn submit_order(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<OrderResult, String>;

    /// Whether we're in paper mode (no real orders submitted).
    fn is_paper_mode(&self) -> bool;
}
