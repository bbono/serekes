use crate::types::{OrderIntent, TokenDirection, Trade};
use log::{debug, error, warn};
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{Amount, OrderStatusType, Side};
use polymarket_client_sdk::types::{Decimal, U256};
use std::str::FromStr;

use super::StrategyEngine;

impl StrategyEngine {
    pub(super) async fn try_order(&mut self, ctx: &crate::types::TickContext) -> Option<Trade> {
        
        let market = ctx.market.as_ref()?;
        let (direction, intent) = self.strategy.create_order(ctx)?;

        if intent.side() == Side::Buy {
            let budget = *self.budget.lock().unwrap_or_else(|e| e.into_inner());
            let cost = intent.cost();
            if cost > budget {
                warn!("Insufficient budget: cost={:.2} budget={:.2}", cost, budget);
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
                warn!("Order size below minimum: size={:.4} min={:.4}", size, min_size);
                return None;
            }
        }

        let token_id = match direction {
            TokenDirection::Up => market.up.token_id.clone(),
            TokenDirection::Down => market.down.token_id.clone(),
        };

        // --- Submit or simulate ---
        let (order_id, order_status, making, taking): (String, OrderStatusType, Decimal, Decimal) =
            if !self.paper_mode {
                let time_from_last_failed_order_ms =
                    crate::common::time::now_ms() - self.last_try_order_failed_timestamp_ms;
                if time_from_last_failed_order_ms <= 3000 {
                    warn!(
                        "Order cooldown: {:?} {}ms since last failure",
                        direction, time_from_last_failed_order_ms
                    );
                    return None;
                }
                match self.sign_and_submit(&token_id, &intent).await {
                    Ok(resp) => {
                        match &resp.status {
                            OrderStatusType::Matched => {}
                            OrderStatusType::Delayed => {
                                debug!("Order {} delayed: making={} taking={}", resp.order_id, resp.making_amount, resp.taking_amount);
                            }
                            OrderStatusType::Unmatched => {
                                debug!("Order {} unmatched: making={} taking={}", resp.order_id, resp.making_amount, resp.taking_amount);
                            }
                            OrderStatusType::Live => {}
                            _ => {
                                warn!("Unexpected order status: {:?} id={}", resp.status, resp.order_id);
                            }
                        }
                        self.last_try_order_failed_timestamp_ms = 0;
                        (
                            resp.order_id,
                            resp.status,
                            resp.making_amount,
                            resp.taking_amount,
                        )
                    }
                    Err(e) => {
                        self.last_try_order_failed_timestamp_ms = crate::common::time::now_ms();
                        error!("Order submit failed: {:?} {e}", direction);
                        return None;
                    }
                }
            } else {
                (
                    "PAPERTRADE-1".to_string(),
                    OrderStatusType::Matched,
                    Decimal::default(),
                    Decimal::default(),
                )
            };

        // --- Resolve fill price/size ---
        let (price, size) = if self.paper_mode {
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
        } else if order_status == OrderStatusType::Matched
            && matches!(intent, OrderIntent::Market { .. })
        {
            let making_f64: f64 = making.try_into().unwrap_or(0.0);
            let taking_f64: f64 = taking.try_into().unwrap_or(0.0);
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
            order_id,
            order_status,
            timestamp_ms: crate::common::time::now_ms(),
        };
        self.trades.push(trade.clone());
        Some(trade)
    }

    async fn sign_and_submit(
        &self,
        token_id: &str,
        intent: &OrderIntent,
    ) -> Result<PostOrderResponse, String> {
        let (Some(ref client), Some(ref signer)) = (&self.client, &self.signer_instance) else {
            return Err("client not initialized".into());
        };

        let token_u256 =
            U256::from_str(token_id).map_err(|e| format!("Invalid token_id: {:?}", e))?;

        let order = match intent {
            OrderIntent::Limit {
                side,
                price,
                size,
                order_type,
            } => client
                .limit_order()
                .order_type(order_type.clone())
                .token_id(token_u256)
                .side(*side)
                .price(*price)
                .size(*size)
                .build()
                .await
                .map_err(|e| format!("Order build failed: {:?}", e))?,
            OrderIntent::Market {
                side,
                amount,
                order_type,
            } => {
                let amt = if *side == polymarket_client_sdk::clob::types::Side::Buy {
                    Amount::usdc(*amount)
                } else {
                    Amount::shares(*amount)
                }
                .map_err(|e| format!("Invalid amount: {:?}", e))?;

                client
                    .market_order()
                    .order_type(order_type.clone())
                    .token_id(token_u256)
                    .side(*side)
                    .amount(amt)
                    .build()
                    .await
                    .map_err(|e| format!("Order build failed: {:?}", e))?
            }
        };

        let signed_order = client
            .sign(signer, order)
            .await
            .map_err(|e| format!("Order sign failed: {:?}", e))?;
        client
            .post_order(signed_order)
            .await
            .map_err(|e| format!("Order post failed: {:?}", e))
    }
}
