mod types;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::types::U256;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

static POLYMARKET_TIME_OFFSET_MS: AtomicI64 = AtomicI64::new(i64::MIN);
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use types::Market;
use url::Url;

mod config;
mod engine;
mod strategies;

use crate::config::AppConfig;
use crate::engine::StrategyEngine;
use crate::strategies::BonoStrategy;
use tokio::sync::watch;

fn backoff_secs(attempt: u32) -> u64 {
    (5u64 << attempt.min(4)).min(60)
}

pub(crate) fn now_ms() -> i64 {
    let offset = POLYMARKET_TIME_OFFSET_MS.load(Ordering::Relaxed);
    if offset == i64::MIN {
        panic!("now_ms() called before Polymarket time offset was fetched");
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + offset
}

fn push_history(history: &Mutex<VecDeque<(f64, i64)>>, price: f64, ts: i64, window_ms: i64) {
    let mut hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.push_back((price, ts));
    let cutoff = ts - window_ms;
    while hist.front().is_some_and(|(_, t)| *t < cutoff) {
        hist.pop_front();
    }
}

fn parse_json(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

fn load_private_key(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                None
            } else {
                info!("Loaded private key from {}", path);
                Some(key)
            }
        }
        Err(_) => {
            debug!("Key file not found: {}", path);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket feed spawners
// ---------------------------------------------------------------------------

fn spawn_binance_ws(
    url: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse(&url).unwrap()).await {
                Ok((ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Binance WS");
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let price = parse_json(&text).and_then(|j| {
                                    let ts = j["T"].as_i64().unwrap_or_else(now_ms);
                                    j["p"].as_str()?.parse::<f64>().ok().map(|p| (p, ts))
                                });
                                if let Some((price, ts)) = price {
                                    let _ = tx.send((price, ts));
                                    push_history(&history, price, ts, window_ms);
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
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
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let price = parse_json(&text)
                                    .and_then(|j| j["price"].as_str()?.parse::<f64>().ok());
                                if let Some(price) = price {
                                    let _ = tx.send((price, now_ms()));
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
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

fn spawn_deribit_dvol_ws(
    asset: String,
    tx: watch::Sender<(f64, i64)>,
    history: Arc<Mutex<VecDeque<(f64, i64)>>>,
    window_ms: i64,
) {
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://www.deribit.com/ws/api/v2").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    info!("Connected to Deribit DVOL WS");
                    let sub = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/subscribe",
                        "id": 1,
                        "params": {
                            "channels": [format!("deribit_volatility_index.{}_usd", asset)]
                        }
                    });
                    let _ = ws_stream.send(Message::Text(sub.to_string())).await;
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let vol = parse_json(&text).and_then(|j| {
                                    (j["method"] == "subscription").then(|| {
                                        let data = &j["params"]["data"];
                                        let v = data["volatility"].as_f64()?;
                                        let ts = data["timestamp"].as_i64().unwrap_or_else(now_ms);
                                        Some((v, ts))
                                    })?
                                });
                                if let Some((val, ts)) = vol {
                                    let _ = tx.send((val, ts));
                                    push_history(&history, val, ts, window_ms);
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
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
    window_ms: i64,
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
                    let (mut write, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        match msg {
                            Message::Text(text) => {
                                let tick = parse_json(&text).and_then(|j| {
                                    let p = j["payload"]["value"].as_f64()?;
                                    let ts = j["payload"]["timestamp"].as_i64()?;
                                    Some((p, ts))
                                });
                                if let Some((price, ts)) = tick {
                                    let _ = tx.send((price, ts));
                                    push_history(&history, price, ts, window_ms);
                                }
                            }
                            Message::Ping(data) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
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

// ---------------------------------------------------------------------------
// Time sync
// ---------------------------------------------------------------------------

async fn fetch_time_offset_ms() -> i64 {
    let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();
    let local_ms = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    match clob.server_time().await {
        Ok(server_ts) => {
            let offset_ms: i64 = (server_ts * 1000) - local_ms();
            POLYMARKET_TIME_OFFSET_MS.store(offset_ms, Ordering::Relaxed);
            info!(
                "Time sync: server={}s local={}ms offset={}ms",
                server_ts,
                local_ms(),
                offset_ms
            );
            if offset_ms.abs() > 300_000 {
                warn!(
                    "Large time skew detected! Adjusting timestamps by {}ms",
                    offset_ms
                );
            }
            offset_ms
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
    let asset_upper = asset.to_uppercase();
    info!("-> Fetching active {} market...", asset_upper);

    let interval_ms = (interval_minutes as i64) * 60_000;
    let kline_interval = if interval_minutes >= 60 {
        format!("{}h", interval_minutes / 60)
    } else {
        format!("{}m", interval_minutes)
    };

    let bucket_start_ms = (now_ms() / interval_ms) * interval_ms;

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

        if up_token.is_empty() || down_token.is_empty() || expires_at_ms <= now_ms() {
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
// Strike price lookup from chainlink history
// ---------------------------------------------------------------------------

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
// Wait for all feeds to have data
// ---------------------------------------------------------------------------

async fn wait_for_feeds<S: crate::types::Strategy>(engine: &StrategyEngine<S>) {
    info!("Waiting for price feeds...");
    loop {
        let b = engine.binance_rx.borrow().0;
        let c = engine.coinbase_rx.borrow().0;
        let cl = engine.chainlink_rx.borrow().0;
        let d = engine.dvol_rx.borrow().0;
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
    let bot_name = config.name.clone();
    env_logger::Builder::new()
        .parse_filters(&config.log_level)
        .format(move |buf, record| {
            use std::io::Write;
            let module = record.file().unwrap_or("unknown");
            let module = module
                .strip_prefix("src/")
                .unwrap_or(module)
                .strip_suffix(".rs")
                .unwrap_or(module)
                .replace('/', "::");
            use env_logger::fmt::Color;
            let level = record.level();
            let mut level_style = buf.style();
            match level {
                log::Level::Error => {
                    level_style.set_color(Color::Red).set_bold(true);
                }
                log::Level::Warn => {
                    level_style.set_color(Color::Yellow).set_bold(true);
                }
                log::Level::Info => {
                    level_style.set_color(Color::Green);
                }
                log::Level::Debug => {
                    level_style.set_color(Color::Cyan);
                }
                log::Level::Trace => {
                    level_style.set_color(Color::Magenta);
                }
            };
            writeln!(
                buf,
                "{} {}::{} {} {}",
                buf.timestamp(),
                bot_name,
                module,
                level_style.value(format_args!("{:5}", level)),
                level_style.value(record.args())
            )
        })
        .init();

    // --- Time synchronization (before anything that uses now_ms) ---
    fetch_time_offset_ms().await;

    let market_asset = config.market.asset.to_lowercase();
    let private_key = load_private_key(&config.wallet.key_file);
    let paper_mode = private_key.is_none();
    let mode = if paper_mode { "paper" } else { "live" };
    info!(
        "Starting Serekeš [{}] | mode={}",
        market_asset.to_uppercase(),
        mode
    );

    // --- Data broadcast channels ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let (dvol_tx, dvol_rx) = watch::channel((0.0f64, 0i64));
    let shared_market: Arc<Mutex<Option<Market>>> = Arc::new(Mutex::new(None));

    // --- Spawn all price feed WebSockets ---
    let binance_ws_url = format!(
        "wss://stream.binance.com:9443/ws/{}usdt@aggTrade",
        market_asset
    );
    let market_window_secs = (config.market.interval_minutes * 60) as i64;
    let binance_history_ms = config
        .engine
        .binance_history_secs
        .map(|s| s as i64 * 1000)
        .unwrap_or(market_window_secs * 1000);
    let chainlink_history_ms = config
        .engine
        .chainlink_history_secs
        .map(|s| s as i64 * 1000)
        .unwrap_or(market_window_secs * 1000);
    let dvol_history_ms = config
        .engine
        .dvol_history_secs
        .map(|s| s as i64 * 1000)
        .unwrap_or(market_window_secs * 1000);

    let binance_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_binance_ws(
        binance_ws_url,
        binance_tx,
        binance_history.clone(),
        binance_history_ms,
    );
    spawn_coinbase_ws(format!("{}-USD", market_asset.to_uppercase()), coinbase_tx);

    let dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_deribit_dvol_ws(
        market_asset.clone(),
        dvol_tx,
        dvol_history.clone(),
        dvol_history_ms,
    );

    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_chainlink_ws(
        market_asset.clone(),
        chainlink_tx,
        chainlink_history.clone(),
        chainlink_history_ms,
    );

    // --- Strategy engine ---
    let strategy = BonoStrategy::new();
    let mut engine = StrategyEngine::new(
        strategy,
        paper_mode,
        config.engine.clone(),
        binance_rx,
        coinbase_rx,
        chainlink_rx,
        dvol_rx,
        shared_market.clone(),
        binance_history.clone(),
        chainlink_history.clone(),
        dvol_history.clone(),
    );

    if let Some(pk) = private_key {
        if engine.initialize_client(&pk).await {
            info!("Polymarket SDK authenticated.");
        }
    }

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let interval_minutes = config.market.interval_minutes;

    tokio::select! {
        _ = run_bot_loop(
            &mut engine,
            &market_asset,
            interval_minutes,
            &shared_market,
            &chainlink_history,
            &binance_history,
        ) => {},
        _ = tokio::signal::ctrl_c() => info!("SIGINT received. Shutting down."),
        _ = sigterm.recv() => info!("SIGTERM received. Shutting down."),
    }
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

async fn run_bot_loop<S: crate::types::Strategy>(
    engine: &mut StrategyEngine<S>,
    asset: &str,
    interval_minutes: u32,
    shared_market: &Arc<Mutex<Option<Market>>>,
    chainlink_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
    binance_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
) {
    wait_for_feeds(engine).await;

    loop {
        let mut market = discover_market(asset, interval_minutes).await;

        match resolve_strike_prices(chainlink_history, binance_history, &market).await {
            Some((chainlink_strike, binance_strike)) => {
                market.strike_price = chainlink_strike;
                market.strike_price_binance = binance_strike;
                info!(
                    "Strike prices for market {}: chainlink={:.2} binance={:.2}",
                    market.slug, chainlink_strike, binance_strike
                );
            }
            None => {
                let wait_ms = market.expires_at_ms.saturating_sub(now_ms()).max(1000) as u64;
                warn!("<- No strike price for market {}. Skipping.", market.slug);
                sleep(Duration::from_millis(wait_ms)).await;
                continue;
            }
        }

        let price_handle = connect_poly_price_ws(shared_market, &market).await;
        run_market_ticks(engine, &market).await;
        price_handle.abort();
    }
}

async fn discover_market(asset: &str, interval_minutes: u32) -> Market {
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

async fn resolve_strike_prices(
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
        if now_ms() > deadline_ms {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn connect_poly_price_ws(
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

async fn run_market_ticks<S: crate::types::Strategy>(
    engine: &mut StrategyEngine<S>,
    market: &Market,
) {
    info!("--> Entering market {}.", market.slug);
    loop {
        if now_ms() > market.expires_at_ms + 1000 {
            break;
        }

        engine.execute_tick().await;

        tokio::task::yield_now().await;
    }

    // Clear the engine state
    engine.clear_state();
    
    info!("<-- Exiting market {}.", market.slug);
}
