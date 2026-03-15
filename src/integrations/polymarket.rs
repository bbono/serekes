use futures_util::StreamExt;
use log::{debug, error, warn};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::types::U256;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

use super::notion::NotionApi;
use crate::types::Market;

// ---------------------------------------------------------------------------
// Market discovery
// ---------------------------------------------------------------------------

pub async fn discover_market(asset: &str, interval_minutes: u32) -> Market {
    loop {
        match fetch_active_market(asset, interval_minutes).await {
            Ok(market) => return market,
            Err(e) => {
                warn!("Market discovery error: {}. Retrying...", e);
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn fetch_active_market(
    asset: &str,
    interval_minutes: u32,
) -> Result<Market, Box<dyn std::error::Error + Send + Sync>> {
    let asset_upper = asset.to_uppercase();
    let interval_ms = (interval_minutes as i64) * 60_000;
    let now_ms = crate::common::time::now_ms();
    let candidate_slugs = Market::candidate_slugs(asset, interval_minutes, now_ms);
    let bucket_start_ms = Market::bucket_start_ms(now_ms, interval_minutes);

    let gamma_client = gamma::Client::default();

    for (i, slug) in candidate_slugs.iter().enumerate() {
        let started_at_ms = bucket_start_ms + (i as i64) * interval_ms;

        let request = gamma::types::request::EventBySlugRequest::builder()
            .slug(slug)
            .build();
        let event = match gamma_client.event_by_slug(&request).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        let market = match event.markets.as_deref().and_then(|m| m.first()) {
            Some(m) => m,
            None => continue,
        };

        let (token_ids, outcomes_list) = match (&market.clob_token_ids, &market.outcomes) {
            (Some(t), Some(o)) => (t, o),
            _ => continue,
        };

        let tokens: Vec<String> = token_ids.iter().map(|id| id.to_string()).collect();
        let (up_token, down_token) = extract_up_down_tokens(outcomes_list, &tokens);

        let expires_at_ms = market.end_date.map(|d| d.timestamp_millis()).unwrap_or(0);

        if up_token.is_empty() || down_token.is_empty() || expires_at_ms <= crate::common::time::now_ms() {
            continue;
        }

        let tick_size: f64 = market
            .order_price_min_tick_size
            .and_then(|d| d.try_into().ok())
            .unwrap_or(0.01);
        let min_order_size: f64 = market
            .order_min_size
            .and_then(|d| d.try_into().ok())
            .unwrap_or(0.0);

        debug!("Found market {} tick_size={} min_order_size={}", slug, tick_size, min_order_size);
        return Ok(Market::new(
            slug.clone(),
            up_token,
            down_token,
            started_at_ms,
            expires_at_ms,
            tick_size,
            min_order_size,
        ));
    }

    Err(format!(
        "<- No active {} {}m market found",
        asset_upper, interval_minutes
    )
    .into())
}

fn extract_up_down_tokens(outcomes: &[String], tokens: &[String]) -> (String, String) {
    let mut up = String::new();
    let mut down = String::new();
    for (i, outcome) in outcomes.iter().enumerate() {
        if outcome.eq_ignore_ascii_case("UP") || outcome.eq_ignore_ascii_case("YES") {
            if let Some(t) = tokens.get(i) {
                up = t.clone();
            }
        } else if outcome.eq_ignore_ascii_case("DOWN") || outcome.eq_ignore_ascii_case("NO") {
            if let Some(t) = tokens.get(i) {
                down = t.clone();
            }
        }
    }
    (up, down)
}

// ---------------------------------------------------------------------------
// History lookup (generic infrastructure — callers decide the match mode)
// ---------------------------------------------------------------------------

/// Look up a price from a history buffer by timestamp.
/// If `exact` is true, requires an exact timestamp match;
/// otherwise returns the latest price at or before `target_ms`.
pub fn lookup_history(history: &Arc<Mutex<VecDeque<(f64, i64)>>>, target_ms: i64, exact: bool) -> f64 {
    let hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.iter()
        .rev()
        .find(|(_, ts)| {
            if exact {
                *ts == target_ms
            } else {
                *ts <= target_ms
            }
        })
        .map(|(price, _)| *price)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Polymarket price WS (per-market)
// ---------------------------------------------------------------------------

pub async fn connect_poly_price_ws(
    shared_market: &Arc<Mutex<Option<Arc<Market>>>>,
    market: &Market,
) -> tokio::task::JoinHandle<()> {
    let token_ids: Vec<U256> = vec![
        U256::from_str(&market.up.token_id).expect("invalid up token_id"),
        U256::from_str(&market.down.token_id).expect("invalid down token_id"),
    ];
    debug!(
        "Connecting to Polymarket Price WS for market {}...",
        market.slug
    );
    *shared_market.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(market.clone()));
    let connected = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_poly_price_ws(shared_market.clone(), token_ids, connected.clone());
    connected.notified().await;
    handle
}

fn spawn_poly_price_ws(
    shared: Arc<Mutex<Option<Arc<Market>>>>,
    token_ids: Vec<U256>,
    connected: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let ws_client = PolyWsClient::default();
            let stream = match ws_client.subscribe_prices(token_ids.clone()) {
                Ok(s) => s,
                Err(e) => {
                    error!("Polymarket Price WS subscribe error: {}. Retrying...", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            debug!("Connected to Polymarket Price WS");
            connected.notify_one();
            let mut stream = Box::pin(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(price_change) => {
                        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(current) = guard.as_ref() {
                            let mut updated = (**current).clone();
                            let ts_ms = price_change.timestamp * 1000;
                            let mut changed = false;
                            for entry in &price_change.price_changes {
                                let asset_str = entry.asset_id.to_string();
                                let is_up = asset_str == updated.up.token_id;
                                let is_down = asset_str == updated.down.token_id;
                                if !is_up && !is_down {
                                    continue;
                                }

                                let side = if is_up { &mut updated.up } else { &mut updated.down };

                                if let Some(bid) = &entry.best_bid {
                                    let v: f64 = bid.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_bid = v;
                                        changed = true;
                                    }
                                }
                                if let Some(ask) = &entry.best_ask {
                                    let v: f64 = ask.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_ask = v;
                                        changed = true;
                                    }
                                }
                                if changed {
                                    side.last_updated = ts_ms;
                                }
                            }
                            if changed {
                                *guard = Some(Arc::new(updated));
                            }
                        }
                    }
                    Err(e) => {
                        error!("Polymarket Price WS error: {}. Reconnecting...", e);
                        break;
                    }
                }
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Market resolver (background task)
// ---------------------------------------------------------------------------

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

        debug!("Resolver found {} completed records to check", records.len());

        for record in &records {
            if let Some(outcome) = self.check_resolution(&record.slug).await {
                let pnl = Market::compute_pnl(&outcome, record.shares_up, record.shares_down, record.cost);
                let pnl_str = format!("{:.2}", pnl);
                debug!("Market {} resolved: {} pnl={}", record.slug, outcome, pnl_str);

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
