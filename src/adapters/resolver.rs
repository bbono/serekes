use log::{debug, error, warn};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

use super::notion::NotionApi;
use crate::domain::Market;

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";
const REQUEST_DELAY_SECS: u64 = 1;

pub fn spawn_resolver(notion: NotionApi, interval_secs: u64) {
    tokio::spawn(async move {
        let resolver = Resolver {
            client: Client::new(),
            notion,
            interval_secs,
        };
        resolver.run().await;
    });

    debug!("Polymarket resolver started");
}

struct Resolver {
    client: Client,
    notion: NotionApi,
    interval_secs: u64,
}

impl Resolver {
    async fn run(&self) {
        loop {
            self.resolve_completed_markets().await;
            sleep(Duration::from_secs(self.interval_secs)).await;
        }
    }

    async fn resolve_completed_markets(&self) {
        let records = match self.notion.find_by_status("completed").await {
            Some(r) => r,
            None => return,
        };

        debug!(
            "Resolver found {} completed records to check",
            records.len()
        );

        for record in &records {
            if let Some(outcome) = self.check_resolution(&record.slug).await {
                let pnl = Market::compute_pnl(
                    &outcome,
                    record.shares_up,
                    record.shares_down,
                    record.cost,
                );
                let pnl_str = format!("{:.2}", pnl);
                debug!(
                    "Market {} resolved: {} pnl={}",
                    record.slug, outcome, pnl_str
                );

                let props = HashMap::from([
                    ("status".to_string(), "resolved".to_string()),
                    ("outcome".to_string(), outcome),
                    ("pnl".to_string(), pnl_str),
                ]);
                self.notion.save(&record.slug, &props).await;
            }

            sleep(Duration::from_secs(REQUEST_DELAY_SECS)).await;
        }
    }

    async fn check_resolution(&self, slug: &str) -> Option<String> {
        let url = format!("{}/markets/slug/{}", GAMMA_API_BASE, slug);
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                error!("Gamma API request failed for {}: {}", slug, e);
                return None;
            }
        };

        if !resp.status().is_success() {
            warn!("Gamma API error for {}: {}", slug, resp.status());
            return None;
        }

        let data: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                error!("Gamma API parse error for {}: {}", slug, e);
                return None;
            }
        };

        if data["closed"].as_bool() != Some(true) {
            return None;
        }

        let outcomes_str = data["outcomes"].as_str()?;
        let prices_str = data["outcomePrices"].as_str()?;

        let outcomes: Vec<String> = serde_json::from_str(outcomes_str).ok()?;
        let prices: Vec<String> = serde_json::from_str(prices_str).ok()?;

        if outcomes.len() != prices.len() {
            return None;
        }

        for (outcome, price) in outcomes.iter().zip(prices.iter()) {
            if price == "1" {
                return Some(outcome.clone());
            }
        }

        None
    }
}
