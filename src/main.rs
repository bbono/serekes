mod common;
mod engine;
mod feeds;
mod strategy;
mod types;

use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{debug, error, info, warn};
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::POLYGON;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{sleep, Duration};

use common::config::AppConfig;
use engine::StrategyEngine;
use feeds::{
    connect_poly_price_ws, discover_market, resolve_strike_prices, spawn_binance_ws,
    spawn_chainlink_ws, spawn_coinbase_ws, spawn_deribit_dvol_ws,
};
use types::Market;

#[tokio::main]
async fn main() {
    let config = AppConfig::load("config.toml");
    common::logger::init(&config.bot.name, &config.logger);

    let market_asset = config.market.asset.to_lowercase();
    let private_key = load_private_key(&config.bot.key_file);
    let paper_mode = private_key.is_none();
    let mode = if paper_mode { "paper" } else { "live" };
    info!(
        "Starting Serekeš [{}] | mode={}",
        market_asset.to_uppercase(),
        mode
    );

    // --- Time synchronization (before anything that uses now_ms) ---
    common::time::fetch_time_offset_ms().await;

    // --- Data broadcast channels ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let (dvol_tx, dvol_rx) = watch::channel((0.0f64, 0i64));
    let shared_market: Arc<Mutex<Option<Market>>> = Arc::new(Mutex::new(None));

    // --- Spawn all price feed WebSockets ---
    let market_window_secs = (config.market.interval_minutes * 60) as i64;

    let binance_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_binance_ws(
        &market_asset,
        binance_tx,
        binance_history.clone(),
        config.feeds.binance_history_ms(market_window_secs),
    );
    spawn_coinbase_ws(&market_asset, coinbase_tx);

    let dvol_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_deribit_dvol_ws(
        market_asset.clone(),
        dvol_tx,
        dvol_history.clone(),
        config.feeds.dvol_history_ms(market_window_secs),
    );

    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    spawn_chainlink_ws(
        market_asset.clone(),
        chainlink_tx,
        chainlink_history.clone(),
        config.feeds.chainlink_history_ms(market_window_secs),
    );

    // --- Strategy engine ---
    let mut engine = StrategyEngine::new(
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
        if let Some((client, signer)) = authenticate_client(&pk).await {
            engine.set_client(client, signer);
            info!("Polymarket SDK authenticated.");
        }
    }

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let interval_minutes = config.market.interval_minutes;

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
        info!("{} received. Shutting down gracefully...", signal);
        info!("Shutdown complete.");
    }
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
                let wait_ms = market
                    .expires_at_ms
                    .saturating_sub(common::time::now_ms())
                    .max(1000) as u64;
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

// ---------------------------------------------------------------------------
// Market tick loop
// ---------------------------------------------------------------------------

async fn run_market_ticks(engine: &mut StrategyEngine, market: &Market) {
    info!("--> Entering market {}.", market.slug);
    loop {
        if common::time::now_ms() > market.expires_at_ms + 1000 {
            break;
        }

        engine.execute_tick().await;

        tokio::task::yield_now().await;
    }

    engine.clear_state();

    info!("<-- Exiting market {}.", market.slug);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

async fn authenticate_client(
    private_key: &str,
) -> Option<(
    polymarket_client_sdk::clob::Client<
        polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>,
    >,
    PrivateKeySigner,
)> {
    let signer: PrivateKeySigner = LocalSigner::from_str(private_key)
        .unwrap()
        .with_chain_id(Some(POLYGON));
    let client_builder =
        ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();

    match client_builder
        .authentication_builder(&signer)
        .signature_type(SignatureType::Proxy)
        .authenticate()
        .await
    {
        Ok(client) => Some((client, signer)),
        Err(e) => {
            error!("SDK auth failed: {}", e);
            None
        }
    }
}

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
