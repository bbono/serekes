use crate::engine::traits::{MarketOrderType, OrderParams, Strategy, TickContext};
use crate::types::{Market, TokenDirection};
use serde::Deserialize;

fn default_delay_secs() -> f64 {
    10.0
}
fn default_budget() -> f64 {
    3.0
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct BonoStrategyConfig {
    #[serde(default = "default_delay_secs")]
    pub delay_secs: f64,
    #[serde(default = "default_budget")]
    pub budget: f64,
}

impl Default for BonoStrategyConfig {
    fn default() -> Self {
        Self {
            delay_secs: default_delay_secs(),
            budget: default_budget(),
        }
    }
}

#[allow(dead_code)]
pub struct BonoStrategy {
    config: BonoStrategyConfig,
}

impl BonoStrategy {
    pub fn new(config: BonoStrategyConfig) -> Self {
        Self { config }
    }
}

impl Strategy for BonoStrategy {
    fn check_entry(
        &self,
        ctx: &TickContext,
        market: &Market,
    ) -> Option<(TokenDirection, OrderParams)> {

        /*let up_mid = (market.up.best_bid + market.up.best_ask) / 2.0;
        let down_mid = (market.down.best_bid + market.down.best_ask) / 2.0;
         log::info!(
            "price: up(bid={:.4} ask={:.4}) down(bid={:.4} ask={:.4})",
            market.up.best_bid, market.up.best_ask,
            market.down.best_bid, market.down.best_ask,
        ); */

        /* return  None; */
        // Wait for delay after market start
        let elapsed_ms = ctx.now_ms - market.started_at_ms;
        if elapsed_ms < (self.config.delay_secs * 1000.0) as i64 {
            return None;
        }

        let up_price = market.up.best_ask;
        let down_price = market.down.best_ask;
        if up_price <= 0.0 || down_price <= 0.0 {
            return None;
        }

        // Buy whichever side is priced higher
        let (direction, price) = if up_price >= down_price {
            (TokenDirection::Up, up_price)
        } else {
            (TokenDirection::Down, down_price)
        };

        log::info!(
            "Entry: {:?} @ ${:.4} (up={:.4} down={:.4})",
            direction,
            price,
            up_price,
            down_price
        );
        Some((direction, OrderParams::Market {
            amount: 1.0,
            price: Some(price),
            order_type: MarketOrderType::FOK
        }))
    }
}
