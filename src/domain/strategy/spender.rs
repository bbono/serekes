use super::Strategy;
use crate::domain::{OrderIntent, TickContext, TokenDirection};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::Decimal;

/// Buys Up for 1 USDC once per market. No logic, just spends.
pub struct SpenderStrategy;

impl SpenderStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Strategy for SpenderStrategy {
    fn create_order(&self, ctx: &TickContext) -> Option<(TokenDirection, OrderIntent)> {
        ctx.market.as_ref()?;

        Some((
            TokenDirection::Up,
            OrderIntent::Market {
                side: Side::Buy,
                amount: Decimal::new(1, 0),
                order_type: OrderType::FOK,
            },
        ))
    }
}
