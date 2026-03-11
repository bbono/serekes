use serde::Deserialize;
use crate::engine::traits::{OrderParams, Strategy, TickContext};
use crate::types::{Market, TokenDirection};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BonoStrategyConfig {
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
        _ctx: &TickContext,
        _market: &Market,
        _balance: f64,
    ) -> Option<(TokenDirection, OrderParams)> {
        None
    }
}
