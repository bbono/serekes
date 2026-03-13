use crate::common::types::{OrderIntent, Strategy, TickContext, TokenDirection};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::Decimal;

pub struct BonoStrategy;

impl BonoStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Strategy for BonoStrategy {
    fn create_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderIntent)> {
        let market = ctx.market.as_ref()?;
        let up_price = market.up.best_ask;
        let down_price = market.down.best_ask;

        let (direction, price) = if up_price >= down_price {
            (TokenDirection::Up, up_price)
        } else {
            (TokenDirection::Down, down_price)
        };

        if price > 0.30 {
            Some((
                direction,
                OrderIntent::Market {
                    side: Side::Buy,
                    amount: Decimal::new(50, 2), // 0.50 USDC
                    order_type: OrderType::FOK,
                },
            ))
        } else {
            None
        }
    }
}
