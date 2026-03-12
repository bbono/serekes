use crate::types::{OrderParams, TickContext, TokenDirection};

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, order)) to buy.
    fn create_entry_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderParams)>;
}
