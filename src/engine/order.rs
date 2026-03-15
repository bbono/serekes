use crate::domain::{OrderIntent, TokenDirection, Trade};
use log::{debug, error, warn};
use polymarket_client_sdk::clob::types::{OrderStatusType, Side};

use super::StrategyEngine;

impl StrategyEngine {
    pub(super) async fn try_order(&mut self, ctx: &crate::domain::TickContext) -> Option<Trade> {
        let market = ctx.market.as_ref()?;
        let (direction, intent) = self.strategy.create_order(ctx)?;

        if intent.side() == Side::Buy {
            let budget = *self.budget.lock().unwrap_or_else(|e| e.into_inner());
            let cost = intent.cost();
            if cost > budget {
                warn!(
                    "Insufficient budget: cost={:.2} budget={:.2}",
                    cost, budget
                );
                return None;
            }
        }

        // --- Validate minimum order size ---
        // Market orders: Polymarket-wide minimum is 1 USDC (amount).
        // Limit orders: per-market minimum from Gamma API (market.min_order_size, typically 5).
        {
            let (_, size) = intent.price_and_size();
            let min_size = match &intent {
                OrderIntent::Market { .. } => 1.0,
                OrderIntent::Limit { .. } => market.min_order_size,
            };
            if min_size > 0.0 && size < min_size {
                warn!(
                    "Order size below minimum: size={:.4} min={:.4}",
                    size, min_size
                );
                return None;
            }
        }

        let token_id = match direction {
            TokenDirection::Up => market.up.token_id.clone(),
            TokenDirection::Down => market.down.token_id.clone(),
        };

        // --- Submit or simulate ---
        let result = if !self.exchange.is_paper_mode() {
            match self.exchange.submit_order(&token_id, &intent).await {
                Ok(resp) => {
                    match &resp.status {
                        OrderStatusType::Matched => {}
                        OrderStatusType::Delayed => {
                            debug!(
                                "Order {} delayed: making={} taking={}",
                                resp.order_id, resp.making_amount, resp.taking_amount
                            );
                        }
                        OrderStatusType::Unmatched => {
                            debug!(
                                "Order {} unmatched: making={} taking={}",
                                resp.order_id, resp.making_amount, resp.taking_amount
                            );
                        }
                        OrderStatusType::Live => {}
                        _ => {
                            warn!(
                                "Unexpected order status: {:?} id={}",
                                resp.status, resp.order_id
                            );
                        }
                    }
                    resp
                }
                Err(e) => {
                    error!("Order submit failed: {:?} {e}", direction);
                    return None;
                }
            }
        } else {
            crate::ports::OrderResult {
                order_id: "PAPERTRADE-1".to_string(),
                status: OrderStatusType::Matched,
                making_amount: polymarket_client_sdk::types::Decimal::default(),
                taking_amount: polymarket_client_sdk::types::Decimal::default(),
            }
        };

        // --- Resolve fill price/size ---
        let (price, size) = if self.exchange.is_paper_mode() {
            match &intent {
                OrderIntent::Market { amount, .. } => {
                    let ask = match direction {
                        TokenDirection::Up => market.up.best_ask,
                        TokenDirection::Down => market.down.best_ask,
                    };
                    let amt_f64: f64 = (*amount).try_into().unwrap_or(0.0);
                    if ask > 0.0 {
                        (ask, amt_f64 / ask)
                    } else {
                        intent.price_and_size()
                    }
                }
                OrderIntent::Limit { price, size, .. } => {
                    let p: f64 = (*price).try_into().unwrap_or(0.0);
                    let s: f64 = (*size).try_into().unwrap_or(0.0);
                    (p, s)
                }
            }
        } else if result.status == OrderStatusType::Matched
            && matches!(intent, OrderIntent::Market { .. })
        {
            let making_f64: f64 = result.making_amount.try_into().unwrap_or(0.0);
            let taking_f64: f64 = result.taking_amount.try_into().unwrap_or(0.0);
            if taking_f64 > 0.0 {
                (making_f64 / taking_f64, taking_f64)
            } else {
                (0.0, 0.0)
            }
        } else {
            intent.price_and_size()
        };

        let trade = Trade {
            direction,
            intent,
            price,
            size,
            order_id: result.order_id,
            order_status: result.status,
            timestamp_ms: self.clock.now_ms(),
        };
        self.trades.push(trade.clone());
        Some(trade)
    }
}
