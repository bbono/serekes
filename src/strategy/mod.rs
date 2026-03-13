pub mod bono;
pub mod konzerva;

pub use bono::BonoStrategy;
pub use konzerva::KonzervaStrategy;

use crate::types::{OrderIntent, TickContext, TokenDirection};

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, intent)) to place an order.
    fn create_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderIntent)>;
}
