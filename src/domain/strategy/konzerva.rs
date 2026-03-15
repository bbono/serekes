use super::Strategy;
use crate::domain::{OrderIntent, TickContext, TokenDirection};

pub struct KonzervaStrategy;

impl KonzervaStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl Strategy for KonzervaStrategy {
    fn create_order(
        &self,
        _ctx: &TickContext,
    ) -> Option<(TokenDirection, OrderIntent)> {
        None
    }
}
