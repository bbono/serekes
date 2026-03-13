use futures_util::StreamExt;
use log::{error, info, warn};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::types::U256;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

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
    info!("-> Fetching active {} market...", asset_upper);

    let interval_ms = (interval_minutes as i64) * 60_000;
    let kline_interval = if interval_minutes >= 60 {
        format!("{}h", interval_minutes / 60)
    } else {
        format!("{}m", interval_minutes)
    };

    let bucket_start_ms = (crate::common::time::now_ms() / interval_ms) * interval_ms;

    let gamma_client = gamma::Client::default();

    for started_at_ms in [bucket_start_ms, bucket_start_ms + interval_ms] {
        let ts_secs = started_at_ms / 1000;
        let slug = format!("{}-updown-{}-{}", asset, kline_interval, ts_secs);
        info!(
            "Discovering active {} {}m market...",
            asset_upper, interval_minutes
        );

        let request = gamma::types::request::EventBySlugRequest::builder()
            .slug(&slug)
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

        info!(
            "Found {} {}m market {} (tick_size={} min_order_size={})",
            asset_upper, interval_minutes, slug, tick_size, min_order_size
        );
        return Ok(Market::new(
            slug,
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
// Strike price resolution
// ---------------------------------------------------------------------------

pub async fn resolve_strike_prices(
    chainlink_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
    binance_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
    market: &Market,
) -> Option<(f64, f64)> {
    info!("Searching strike prices for market {}...", market.slug);
    let deadline_ms = market.started_at_ms + 10_000;
    let mut chainlink_strike = 0.0f64;
    let mut binance_strike = 0.0f64;
    loop {
        if chainlink_strike == 0.0 {
            chainlink_strike = lookup_strike(chainlink_history, market.started_at_ms, true);
        }
        if binance_strike == 0.0 {
            binance_strike = lookup_strike(binance_history, market.started_at_ms, false);
        }
        if chainlink_strike > 0.0 && binance_strike > 0.0 {
            return Some((chainlink_strike, binance_strike));
        }
        if crate::common::time::now_ms() > deadline_ms {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn lookup_strike(history: &Arc<Mutex<VecDeque<(f64, i64)>>>, started_ms: i64, exact: bool) -> f64 {
    let hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.iter()
        .rev()
        .find(|(_, ts)| {
            if exact {
                *ts == started_ms
            } else {
                *ts <= started_ms
            }
        })
        .map(|(price, _)| *price)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Polymarket price WS (per-market)
// ---------------------------------------------------------------------------

pub async fn connect_poly_price_ws(
    shared_market: &Arc<Mutex<Option<Market>>>,
    market: &Market,
) -> tokio::task::JoinHandle<()> {
    let token_ids: Vec<U256> = vec![
        U256::from_str(&market.up.token_id).expect("invalid up token_id"),
        U256::from_str(&market.down.token_id).expect("invalid down token_id"),
    ];
    info!(
        "Connecting to Polymarket Price WS for market {}...",
        market.slug
    );
    *shared_market.lock().unwrap_or_else(|e| e.into_inner()) = Some(market.clone());
    let connected = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_poly_price_ws(shared_market.clone(), token_ids, connected.clone());
    connected.notified().await;
    handle
}

fn spawn_poly_price_ws(
    shared: Arc<Mutex<Option<Market>>>,
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
            info!("Connected to Polymarket Price WS");
            connected.notify_one();
            let mut stream = Box::pin(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(price_change) => {
                        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(m) = guard.as_mut() {
                            let ts_ms = price_change.timestamp * 1000;
                            for entry in &price_change.price_changes {
                                let asset_str = entry.asset_id.to_string();
                                let is_up = asset_str == m.up.token_id;
                                let is_down = asset_str == m.down.token_id;
                                if !is_up && !is_down {
                                    continue;
                                }

                                let side = if is_up { &mut m.up } else { &mut m.down };

                                if let Some(bid) = &entry.best_bid {
                                    let v: f64 = bid.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_bid = v;
                                    }
                                }
                                if let Some(ask) = &entry.best_ask {
                                    let v: f64 = ask.to_string().parse().unwrap_or(0.0);
                                    if v > 0.0 {
                                        side.best_ask = v;
                                    }
                                }
                                side.last_updated = ts_ms;
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

