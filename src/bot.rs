mod types;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::gamma;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use types::Market;
use url::Url;

mod config;
mod engine;

use crate::config::AppConfig;
use crate::engine::{BonoStrategy, StrategyEngine};
use tokio::sync::watch;

fn backoff_secs(attempt: u32) -> u64 {
    (5u64 << attempt.min(4)).min(60)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// WebSocket feed spawners
// ---------------------------------------------------------------------------

fn spawn_binance_ws(
    url: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    max_entries: usize,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse(&url).unwrap()).await {
                Ok((ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Binance WS");
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(p_str) = json["p"].as_str() {
                                    if let Ok(price) = p_str.parse::<f64>() {
                                        let ts =
                                            json["T"].as_i64().unwrap_or_else(|| now_secs() * 1000);
                                        let _ = tx.send((price, ts));
                                        let mut hist =
                                            history.lock().unwrap_or_else(|e| e.into_inner());
                                        hist.push_back((price, ts));
                                        if hist.len() > max_entries {
                                            hist.pop_front();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Binance WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

fn spawn_coinbase_ws(product: String, tx: watch::Sender<(f64, i64)>) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-feed.exchange.coinbase.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Coinbase WS");
                    let sub = serde_json::json!({
                        "type": "subscribe",
                        "product_ids": [product],
                        "channels": ["ticker"]
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(p_str) = json["price"].as_str() {
                                    if let Ok(price) = p_str.parse::<f64>() {
                                        let ts = now_secs() * 1000;
                                        let _ = tx.send((price, ts));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Coinbase WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

fn spawn_deribit_dvol_ws(asset: String, tx: watch::Sender<f64>) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://www.deribit.com/ws/api/v2").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Deribit DVOL WS");
                    let subscribe_msg = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/subscribe",
                        "id": 1,
                        "params": {
                            "channels": [format!("deribit_volatility_index.{}_usd", asset)]
                        }
                    });
                    let _ = ws_stream
                        .send(Message::Text(subscribe_msg.to_string()))
                        .await;
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if json["method"] == "subscription" {
                                    if let Some(val) = json["params"]["data"]["volatility"].as_f64()
                                    {
                                        let _ = tx.send(val);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Deribit WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

fn spawn_chainlink_ws(
    asset: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    max_entries: usize,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-live-data.polymarket.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Chainlink WS");
                    let sub = serde_json::json!({
                        "action": "subscribe",
                        "subscriptions": [{
                            "topic": "crypto_prices_chainlink",
                            "type": "update",
                            "filters": format!("{{\"symbol\":\"{}/usd\"}}", asset)
                        }]
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let (Some(price), Some(ts)) = (
                                    json["payload"]["value"].as_f64(),
                                    json["payload"]["timestamp"].as_i64(),
                                ) {
                                    let _ = tx.send((price, ts));
                                    let mut hist =
                                        history.lock().unwrap_or_else(|e| e.into_inner());
                                    hist.push_back((price, ts));
                                    if hist.len() > max_entries {
                                        hist.pop_front();
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    error!("Chainlink WS error: {}. Reconnecting in {}s...", e, delay);
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Polymarket price WS (per-market, returns abort-able handle)
// ---------------------------------------------------------------------------

fn spawn_poly_price_ws(
    shared: Arc<Mutex<Option<Market>>>,
    token_ids: Vec<String>,
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
                            for entry in &price_change.price_changes {
                                let bid: f64 = entry
                                    .best_bid
                                    .as_ref()
                                    .map(|v| v.to_string().parse().unwrap_or(0.0))
                                    .unwrap_or(0.0);
                                let ask: f64 = entry
                                    .best_ask
                                    .as_ref()
                                    .map(|v| v.to_string().parse().unwrap_or(0.0))
                                    .unwrap_or(0.0);

                                if bid <= 0.0 || ask <= 0.0 {
                                    continue;
                                }

                                let is_up = entry.asset_id == m.up.token_id;
                                let is_down = entry.asset_id == m.down.token_id;
                                if !is_up && !is_down {
                                    continue;
                                }

                                // Validate: up mid + down mid should be ~1.0
                                let (new_up_mid, new_down_mid) = if is_up {
                                    ((bid + ask) / 2.0, (m.down.best_bid + m.down.best_ask) / 2.0)
                                } else {
                                    ((m.up.best_bid + m.up.best_ask) / 2.0, (bid + ask) / 2.0)
                                };
                                let sum = new_up_mid + new_down_mid;
                                // Skip validation if the other side hasn't been set yet
                                if new_down_mid > 0.0
                                    && new_up_mid > 0.0
                                    && (sum < 0.9 || sum > 1.1)
                                {
                                    continue;
                                }

                                if is_up {
                                    m.up.best_bid = bid;
                                    m.up.best_ask = ask;
                                } else {
                                    m.down.best_bid = bid;
                                    m.down.best_ask = ask;
                                }
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
// Time sync
// ---------------------------------------------------------------------------

async fn sync_time_offset() -> i64 {
    let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();
    match clob.server_time().await {
        Ok(server_ts) => {
            let local_ts = now_secs();
            let offset = server_ts - local_ts;
            info!(
                "Time sync: server={} local={} offset={}s",
                server_ts, local_ts, offset
            );
            if offset.abs() > 300 {
                warn!(
                    "Large time skew detected! Adjusting timestamps by {}s",
                    offset
                );
            }
            offset
        }
        Err(e) => {
            error!("Failed to sync time: {}. Defaulting to 0 offset.", e);
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Market discovery
// ---------------------------------------------------------------------------

async fn fetch_active_market(
    asset: &str,
    interval_minutes: u32,
) -> Result<Market, Box<dyn std::error::Error + Send + Sync>> {
    info!("Fetching active {} market...", asset.to_uppercase());

    let binance_symbol = format!("{}USDT", asset.to_uppercase());
    let interval_secs = (interval_minutes as i64) * 60;
    let kline_interval = if interval_minutes >= 60 {
        format!("{}h", interval_minutes / 60)
    } else {
        format!("{}m", interval_minutes)
    };

    let now_ts = now_secs();
    let bucket_start_ts = (now_ts / interval_secs) * interval_secs;
    let timestamps_to_check = vec![bucket_start_ts, bucket_start_ts + interval_secs];

    let gamma_client = gamma::Client::default();
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    for ts in timestamps_to_check {
        let slug = format!("{}-updown-{}-{}", asset, kline_interval, ts);

        info!(
            "Discovering active {} {}m market...",
            asset.to_uppercase(),
            interval_minutes
        );

        let request = gamma::types::request::EventBySlugRequest::builder()
            .slug(&slug)
            .build();
        let event = match gamma_client.event_by_slug(&request).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        if let Some(markets) = event.markets {
            if let Some(market) = markets.first() {
                let started_at_ms = ts * 1000;
                let expires_at_ms = market.end_date.map(|d| d.timestamp_millis()).unwrap_or(0);

                let strike_binance =
                    fetch_binance_open_price(&http_client, &binance_symbol, &kline_interval, ts)
                        .await;

                if let (Some(tokens_str), Some(outcomes_str)) =
                    (&market.clob_token_ids, &market.outcomes)
                {
                    let tokens: Vec<String> = serde_json::from_str(tokens_str)?;
                    let outcomes: Vec<String> = serde_json::from_str(outcomes_str)?;

                    let (up_token, down_token) = extract_up_down_tokens(&outcomes, &tokens);

                    if !up_token.is_empty()
                        && !down_token.is_empty()
                        && expires_at_ms > now_secs() * 1000
                    {
                        info!(
                            "Found {} {}m market {}",
                            asset.to_uppercase(),
                            interval_minutes,
                            slug
                        );

                        return Ok(Market::new(
                            slug,
                            up_token,
                            down_token,
                            started_at_ms,
                            expires_at_ms,
                            strike_binance,
                        ));
                    }
                }
            }
        }
    }

    Err(format!(
        "No active {} {}m market found",
        asset.to_uppercase(),
        interval_minutes
    )
    .into())
}

async fn fetch_binance_open_price(
    client: &reqwest::Client,
    symbol: &str,
    interval: &str,
    start_ts: i64,
) -> f64 {
    let url = format!(
        "https://api.binance.com/api/v3/klines?symbol={}&interval={}&startTime={}&limit=1",
        symbol,
        interval,
        start_ts * 1000
    );
    if let Ok(resp) = client.get(&url).send().await {
        if resp.status().is_success() {
            let json: serde_json::Value = resp.json().await.unwrap_or_default();
            return json
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|k| k[1].as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
        }
    }
    0.0
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
// Strike price lookup from chainlink history
// ---------------------------------------------------------------------------

fn lookup_chainlink_strike(history: &Arc<Mutex<VecDeque<(f64, i64)>>>, started_ms: i64) -> f64 {
    let hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.iter()
        .rev()
        .find(|(_, ts)| *ts == started_ms)
        .map(|(price, _)| *price)
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Wait for all feeds to have data
// ---------------------------------------------------------------------------

async fn wait_for_feeds<S: crate::engine::traits::Strategy>(engine: &StrategyEngine<S>) {
    info!("Waiting for price feeds...");
    loop {
        let b = engine.binance_rx.borrow().0;
        let c = engine.coinbase_rx.borrow().0;
        let cl = engine.chainlink_rx.borrow().0;
        let d = *engine.dvol_rx.borrow();
        if b > 0.0 && c > 0.0 && cl > 0.0 && d > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    info!("All feeds ready.");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config = AppConfig::load("config.toml");
    env_logger::Builder::new()
        .parse_filters(&config.log_level)
        .format(|buf, record| {
            use std::io::Write;
            let module = record.file().unwrap_or("unknown");
            let module = module
                .strip_prefix("src/")
                .unwrap_or(module)
                .strip_suffix(".rs")
                .unwrap_or(module)
                .replace('/', "::");
            writeln!(
                buf,
                "[{} {:5} {}] {}",
                buf.timestamp(),
                record.level(),
                module,
                record.args()
            )
        })
        .init();

    let market_asset = config.market.asset.to_lowercase();
    let private_key = match std::fs::read_to_string(&config.wallet.key_file) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                None
            } else {
                info!("Loaded private key from {}", config.wallet.key_file);
                Some(key)
            }
        }
        Err(_) => {
            debug!("Key file not found: {}", config.wallet.key_file);
            None
        }
    };

    let paper_mode = private_key.is_none();
    if paper_mode {
        info!("PAPER mode (no private key found)");
    } else {
        info!("LIVE mode (private key loaded)");
    }
    info!(
        "Starting Serekeš [{}] | mode={}",
        market_asset.to_uppercase(),
        if paper_mode { "paper" } else { "live" }
    );

    // --- Data broadcast channels ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let (dvol_tx, dvol_rx) = watch::channel(0.0);
    let shared_market: Arc<Mutex<Option<Market>>> = Arc::new(Mutex::new(None));

    // --- Spawn all price feed WebSockets ---
    let binance_ws_url = format!(
        "wss://stream.binance.com:9443/ws/{}usdt@aggTrade",
        market_asset
    );
    let binance_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_binance_ws(
        binance_ws_url,
        binance_tx,
        binance_history.clone(),
        config.engine.binance_history_max,
    );
    spawn_coinbase_ws(format!("{}-USD", market_asset.to_uppercase()), coinbase_tx);
    spawn_deribit_dvol_ws(market_asset.clone(), dvol_tx);

    let chainlink_history_max = ((config.market.interval_minutes as usize) * 60) + 5;
    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_chainlink_ws(
        market_asset.clone(),
        chainlink_tx,
        chainlink_history.clone(),
        chainlink_history_max,
    );

    // --- Time synchronization ---
    let time_offset = sync_time_offset().await;

    // --- Strategy engine ---
    let strategy = BonoStrategy::new();
    let mut engine = StrategyEngine::new(
        strategy,
        paper_mode,
        time_offset,
        config.engine.clone(),
        binance_rx,
        coinbase_rx,
        chainlink_rx,
        dvol_rx,
        shared_market.clone(),
        binance_history.clone(),
        chainlink_history.clone(),
    );

    if !paper_mode {
        if let Some(pk) = private_key {
            if engine.initialize_client(&pk).await {
                info!("Polymarket SDK authenticated.");
            }
        }
    }

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let interval_minutes = config.market.interval_minutes;

    tokio::select! {
        _ = async {
            wait_for_feeds(&engine).await;

            // === MARKET ROTATION LOOP ===
            loop {
                // 1. Discover market
                let mut market = loop {
                    match fetch_active_market(&market_asset, interval_minutes).await {
                        Ok(result) => break result,
                        Err(e) => {
                            warn!("Market discovery error: {}. Retrying...", e);
                            sleep(Duration::from_secs(5)).await;
                        }
                    }
                };

                // 2. Wait up to 10s for chainlink strike price, then skip market
                info!("Searching strike price for market {}...", market.slug);
                let deadline_ms = market.started_at_ms + 10_000;
                let strike_chainlink = loop {
                    let price = lookup_chainlink_strike(&chainlink_history, market.started_at_ms);
                    if price > 0.0 {
                        break price;
                    }
                    if now_secs() * 1000 > deadline_ms {
                        break 0.0;
                    }
                    sleep(Duration::from_millis(100)).await;
                };
                if strike_chainlink == 0.0 {
                    let wait_secs = (market.expires_at_ms / 1000).saturating_sub(now_secs()).max(1) as u64;
                    warn!("No strike price for market {}. Skipping market.", market.slug);
                    sleep(Duration::from_secs(wait_secs)).await;
                    continue;
                } else {
                    market.strike_price = strike_chainlink;
                    info!("Strike price is {:.2} for market {}", strike_chainlink, market.slug);
                }

                // 3. Spawn Polymarket price WS for this market
                let up_token = market.up.token_id.clone();
                let down_token = market.down.token_id.clone();
                info!("Connecting to Polymarket Price WS for market {}...", market.slug);
                *shared_market.lock().unwrap_or_else(|e| e.into_inner()) = Some(market.clone());
                let poly_connected = Arc::new(tokio::sync::Notify::new());
                let price_handle = spawn_poly_price_ws(
                    shared_market.clone(),
                    vec![up_token, down_token],
                    poly_connected.clone(),
                );
                poly_connected.notified().await;

                // 4. Tick loop until market expires
                info!("--->>> Entering market {}.", market.slug);
                loop {
                    if now_secs() * 1000 > market.expires_at_ms + 1000 {
                        break;
                    }
                    if let Some(trade) = engine.execute_tick().await {
                        info!("Trade: {:?}", trade);
                    }
                    tokio::task::yield_now().await;
                }
                info!("<<<--- Exiting market {}.", market.slug);

                // 6. Cleanup
                price_handle.abort();
            }
        } => {},
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT received. Shutting down.");
        },
        _ = sigterm.recv() => {
            info!("SIGTERM received. Shutting down.");
        }
    }
}
