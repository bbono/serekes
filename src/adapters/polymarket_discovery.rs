use crate::domain::Market;
use crate::ports::{ClockPort, MarketDiscoveryPort};
use log::{debug, warn};
use polymarket_client_sdk::gamma;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Gamma API adapter implementing MarketDiscoveryPort.
pub struct GammaDiscoveryAdapter {
    clock: Arc<dyn ClockPort>,
}

impl GammaDiscoveryAdapter {
    pub fn new(clock: Arc<dyn ClockPort>) -> Self {
        Self { clock }
    }
}

#[async_trait::async_trait]
impl MarketDiscoveryPort for GammaDiscoveryAdapter {
    async fn discover(&self, asset: &str, interval_minutes: u32) -> Market {
        loop {
            match fetch_active_market(asset, interval_minutes, &*self.clock).await {
                Ok(market) => return market,
                Err(e) => {
                    warn!("Market discovery error: {}. Retrying...", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}

async fn fetch_active_market(
    asset: &str,
    interval_minutes: u32,
    clock: &dyn ClockPort,
) -> Result<Market, Box<dyn std::error::Error + Send + Sync>> {
    let asset_upper = asset.to_uppercase();
    let interval_ms = (interval_minutes as i64) * 60_000;
    let now_ms = clock.now_ms();
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

        if up_token.is_empty() || down_token.is_empty() || expires_at_ms <= clock.now_ms() {
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

        debug!(
            "Found market {} tick_size={} min_order_size={}",
            slug, tick_size, min_order_size
        );
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
