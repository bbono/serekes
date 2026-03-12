use crate::types::{OrderParams, TickContext, TokenDirection};

pub trait Strategy {
    /// Called each tick when idle. Return Some((direction, order)) to buy.
    fn create_entry_order(
        &self,
        ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderParams)>;

    /// Called each tick while holding a position.
    /// Return Some(OrderParams) to sell, None to keep holding.
    #[allow(dead_code)]
    fn create_exit_order(
        &self,
        _ctx: &TickContext,
        _position_size: f64,
    ) -> Option<OrderParams> {
        None
    }

    /// Whether this strategy manages its own exit logic.
    /// If false, the engine considers itself done immediately after entry.
    #[allow(dead_code)]
    fn manages_exit(&self) -> bool {
        false
    }
}
