use std::collections::HashMap;

use log::{debug, info, warn};
use tokio::time::{sleep, Duration};

use crate::domain::{MarketSummary, TokenDirection};

use super::StrategyEngine;

impl StrategyEngine {
    /// Main entry point: wait for feeds, then loop over markets forever.
    pub async fn run(
        &mut self,
        asset: &str,
        interval_minutes: u32,
        resolve_strike_price: bool,
        tick_interval_us: u64,
    ) {
        self.wait_for_feeds().await;

        loop {
            let mut market = self.discovery.discover(asset, interval_minutes).await;
            info!("Market discovered: {}", market.slug);

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
                            .saturating_sub(self.clock.now_ms())
                            .max(1000) as u64;
                        warn!("Skipping market {}.", market.slug);
                        sleep(Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                }
            }

            self.persistence
                .save(&market.slug, HashMap::from([("status", "trading")]));

            self.market_price.connect(&market).await;
            let summary = self.trade_market(&market, tick_interval_us).await;
            self.clear_state();
            self.market_price.disconnect();

            let trades_str = summary.trades.to_string();
            let cost_str = format!("{:.2}", summary.cost);
            let shares_up_str = format!("{:.4}", summary.shares_up);
            let shares_down_str = format!("{:.4}", summary.shares_down);
            self.persistence.save(
                &market.slug,
                HashMap::from([
                    ("status", "completed"),
                    ("trades", trades_str.as_str()),
                    ("cost", cost_str.as_str()),
                    ("shares_up", shares_up_str.as_str()),
                    ("shares_down", shares_down_str.as_str()),
                ]),
            );

            let remaining_ms = market.expires_at_ms.saturating_sub(self.clock.now_ms());
            if remaining_ms > 0 {
                sleep(Duration::from_millis((remaining_ms + 1000) as u64)).await;
            }
        }
    }

    async fn trade_market(
        &mut self,
        market: &crate::domain::Market,
        tick_interval_us: u64,
    ) -> MarketSummary {
        info!(
            "Trading market {} (expires in {:.0}s)",
            market.slug,
            market.time_to_expire_ms(self.clock.now_ms()) as f64 / 1000.0
        );
        let mut last_result = None;
        loop {
            if self.clock.now_ms() > market.expires_at_ms + 1000 {
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

        let mut summary = MarketSummary {
            trades: 0,
            cost: 0.0,
            shares_up: 0.0,
            shares_down: 0.0,
        };
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
        info!(
            "Trading completed. Market {} ({} trades, cost={:.2})",
            market.slug, summary.trades, summary.cost
        );
        summary
    }

    async fn wait_for_feeds(&self) {
        loop {
            let b = self.binance_feed.is_ready();
            let c = self.coinbase_feed.is_ready();
            let cl = self.chainlink_feed.is_ready();
            if b && c && cl {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        info!("All feeds ready.");
    }
}
