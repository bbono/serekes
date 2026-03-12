use crate::engine::traits::{MarketOrderType, OrderParams, Strategy, TickContext};
use crate::types::{Market, TokenDirection};

#[allow(dead_code)]
pub struct BonoStrategy {}

impl BonoStrategy {
    pub fn new() -> Self {
        Self {}
    }
}

impl Strategy for BonoStrategy {
    fn create_entry_order(
        &self,
        _ctx: &TickContext,
        market: &Market,
    ) -> Option<(TokenDirection, OrderParams)> {
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
                OrderParams::Market {
                    amount: 1.0, // TODO: Hardcoded
                    order_type: MarketOrderType::FOK,
                },
            ))
        } else {
            None
        }
    }
}
