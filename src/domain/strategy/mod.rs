pub mod lag_exploiter;
pub mod spender;

pub use lag_exploiter::LagExploiterStrategy;
pub use spender::SpenderStrategy;

use super::{OrderIntent, TickContext, TokenDirection};

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, intent)) to place an order.
    fn create_order(&self, ctx: &TickContext) -> Option<(TokenDirection, OrderIntent)>;
}
