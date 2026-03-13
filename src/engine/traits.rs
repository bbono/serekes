use crate::types::{OrderIntent, TickContext, TokenDirection};

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, intent)) to enter.
    fn create_entry_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderIntent)>;
}
