use futures_util::StreamExt;
use log::{debug, error, info, warn};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::types::U256;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};

mod feeds;

use crate::common::config::AppConfig;
use crate::common::types::Market;
use crate::engine::StrategyEngine;
use crate::strategy::{BonoStrategy, KonzervaStrategy};
use feeds::{spawn_binance_ws, spawn_chainlink_ws, spawn_coinbase_ws, spawn_deribit_dvol_ws};

fn create_strategy(name: &str) -> Box<dyn crate::common::types::Strategy> {
    match name {
        "bono" => Box::new(BonoStrategy::new()),
        "konzerva" => Box::new(KonzervaStrategy::new()),
        _ => panic!("Unknown strategy: '{}'. Supported: bono, konzerva", name),
    }
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
            crate::common::time::store_time_offset(offset_ms);
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

async fn wait_for_feeds(engine: &StrategyEngine) {
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
// Entry point
// ---------------------------------------------------------------------------

pub async fn run() {
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

    let market_asset = config.asset.to_lowercase();
    let private_key = load_private_key(&config.key_file);
    let paper_mode = private_key.is_none();
    let mode = if paper_mode { "paper" } else { "live" };
    info!(
        "Starting Serekeš [{}] | mode={}",
        market_asset.to_uppercase(),
        mode
    );

    // --- Time synchronization (before anything that uses now_ms) ---
    fetch_time_offset_ms().await;

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
    let market_window_secs = (config.interval_minutes * 60) as i64;
    let binance_history_ms = config
        .binance_history_secs
        .map(|s| s as i64 * 1000)
        .unwrap_or(market_window_secs * 1000);
    let chainlink_history_ms = config
        .chainlink_history_secs
        .map(|s| s as i64 * 1000)
        .unwrap_or(market_window_secs * 1000);
    let dvol_history_ms = config
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
    let strategy = create_strategy(&config.strategy);
    let mut engine = StrategyEngine::new(
        strategy,
        paper_mode,
        config.clone(),
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

    let interval_minutes = config.interval_minutes;

    let signal_name = tokio::select! {
        _ = run_bot_loop(
            &mut engine,
            &market_asset,
            interval_minutes,
            &shared_market,
            &chainlink_history,
            &binance_history,
        ) => None,
        _ = tokio::signal::ctrl_c() => Some("SIGINT"),
        _ = sigterm.recv() => Some("SIGTERM"),
    };

    if let Some(signal) = signal_name {
        graceful_shutdown(signal).await;
    }
}

async fn graceful_shutdown(
    signal: &str
) {
    info!("{} received. Shutting down gracefully...", signal);
    info!("Shutdown complete.");
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

async fn run_bot_loop(
    engine: &mut StrategyEngine,
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
                let wait_ms = market.expires_at_ms.saturating_sub(crate::common::time::now_ms()).max(1000) as u64;
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
        if crate::common::time::now_ms() > deadline_ms {
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

async fn run_market_ticks(
    engine: &mut StrategyEngine,
    market: &Market,
) {
    info!("--> Entering market {}.", market.slug);
    loop {
        if crate::common::time::now_ms() > market.expires_at_ms + 1000 {
            break;
        }

        engine.execute_tick().await;

        tokio::task::yield_now().await;
    }

    // Clear the engine state
    engine.clear_state();

    info!("<-- Exiting market {}.", market.slug);
}
