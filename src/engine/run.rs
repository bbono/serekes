use std::collections::HashMap;

use log::{debug, warn};
use tokio::time::{sleep, Duration};

use crate::integrations::{connect_poly_price_ws, discover_market};
use crate::integrations::notion::Notion;
use crate::types::{MarketSummary, TokenDirection};

use super::StrategyEngine;

impl StrategyEngine {
    /// Main entry point: wait for feeds, then loop over markets forever.
    pub async fn run(
        &mut self,
        asset: &str,
        interval_minutes: u32,
        resolve_strike_price: bool,
        tick_interval_us: u64,
        notion: &Notion,
    ) {
        self.wait_for_feeds().await;

        loop {
            let mut market = discover_market(asset, interval_minutes).await;

            if !resolve_strike_price {
                debug!("Skipping strike resolution for {}", market.slug);
            }

            if resolve_strike_price {
                match self.resolve_strike_prices(&market).await {
                    Some((chainlink_strike, binance_strike)) => {
                        market.strike_price = chainlink_strike;
                        market.strike_price_binance = binance_strike;
                        debug!(
                            "Strike prices for market {}: chainlink={:.2} binance={:.2}",
                            market.slug, chainlink_strike, binance_strike
                        );
                    }
                    None => {
                        let wait_ms = market
                            .expires_at_ms
                            .saturating_sub(crate::common::time::now_ms())
                            .max(1000) as u64;
                        warn!("Skipping market {}.", market.slug);
                        sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                }
            }

            notion.save(&market.slug, HashMap::from([("status", "trading")]));

            let price_handle = connect_poly_price_ws(&self.shared_market, &market).await;
            let summary = self.trade_market(&market, tick_interval_us).await;
            self.clear_state();
            price_handle.abort();

            let trades_str = summary.trades.to_string();
            let cost_str = format!("{:.2}", summary.cost);
            let shares_up_str = format!("{:.4}", summary.shares_up);
            let shares_down_str = format!("{:.4}", summary.shares_down);
            notion.save(&market.slug, HashMap::from([
                ("status", "completed"),
                ("trades", trades_str.as_str()),
                ("cost", cost_str.as_str()),
                ("shares_up", shares_up_str.as_str()),
                ("shares_down", shares_down_str.as_str()),
            ]));

            let remaining_ms = market.expires_at_ms.saturating_sub(crate::common::time::now_ms());
            if remaining_ms > 0 {
                sleep(Duration::from_millis((remaining_ms + 1000) as u64)).await;
            }
        }
    }

    async fn trade_market(&mut self, market: &crate::types::Market, tick_interval_us: u64) -> MarketSummary {
        debug!(
            "Trading market {} (expires in {:.0}s)",
            market.slug,
            market.time_to_expire_ms() as f64 / 1000.0
        );
        let mut last_result = None;
        loop {
            if crate::common::time::now_ms() > market.expires_at_ms + 1000 {
                break;
            }
            let result = self.execute_tick().await;
            let completed = result.completed;
            last_result = Some(result);
            if completed {
                break;
            }
            tokio::time::sleep(Duration::from_micros(tick_interval_us)).await;
        }

        let mut summary = MarketSummary { trades: 0, cost: 0.0, shares_up: 0.0, shares_down: 0.0 };
        if let Some(result) = last_result {
            for trade in &result.trades {
                summary.trades += 1;
                summary.cost += trade.intent.cost();
                match trade.direction {
                    TokenDirection::Up => summary.shares_up += trade.size,
                    TokenDirection::Down => summary.shares_down += trade.size,
                }
            }
        }
        debug!("Trading completed. Market {} ({} trades)", market.slug, summary.trades);
        summary
    }

    async fn wait_for_feeds(&self) {
        loop {
            let b = self.binance_rx.borrow().0;
            let c = self.coinbase_rx.borrow().0;
            let cl = self.chainlink_rx.borrow().0;
            if b > 0.0 && c > 0.0 && cl > 0.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        debug!("All feeds ready.");
    }
}
