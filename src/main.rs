mod common;
mod engine;
mod feeds;
mod strategy;
mod telegram;
mod types;

use alloy_signer_local::{LocalSigner, PrivateKeySigner};
use log::{debug, error, warn};
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

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let config = AppConfig::load("config.toml");
    common::logger::init(&config.bot.name, &config.logger);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.bot.worker_threads)
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime");

    runtime.block_on(async_main(config));
}

async fn async_main(config: AppConfig) {

    let market_asset = config.market.asset.to_lowercase();
    let private_key = load_private_key(&config.bot.key_file, config.bot.truncate_key_file);
    let paper_mode = private_key.is_none();
    let mode = if paper_mode { "paper" } else { "live" };
    debug!("Starting asset={} mode={}", market_asset.to_uppercase(), mode);

    // --- Time synchronization (before anything that uses now_ms) ---
    common::time::fetch_time_offset_ms().await;

    // --- Data broadcast channels ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let (dvol_tx, dvol_rx) = watch::channel((0.0f64, 0i64));
    let shared_market: Arc<Mutex<Option<Arc<Market>>>> = Arc::new(Mutex::new(None));

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

    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_chainlink_ws(
        market_asset.clone(),
        chainlink_tx,
        chainlink_history.clone(),
        config.feeds.chainlink_history_ms(market_window_secs),
    );

    // --- Budget (shared, safe to update at runtime) ---
    let budget: Arc<Mutex<f64>> = Arc::new(Mutex::new(config.bot.initial_budget));

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
        budget.clone(),
    );

    if let Some(pk) = private_key {
        if let Some((client, signer)) = authenticate_client(&pk).await {
            engine.set_client(client, signer);
            debug!("SDK authenticated");
        }
    }

    // --- Telegram ---
    let mut cmds = telegram::commands::Commands::new();
    let budget_ref = budget.clone();
    cmds.register("budget", "Budget", move |args| bot_budget_command(&budget_ref, args));
    let tg = telegram::spawn(&config.telegram, cmds.build());
    tg.send(format!("{} started.", config.bot.name));

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let interval_minutes = config.market.interval_minutes;

    let resolve_strike_price = config.market.resolve_strike_price;
    let signal_name = tokio::select! {
        _ = run_bot_loop(
            &mut engine,
            &market_asset,
            interval_minutes,
            resolve_strike_price,
            &shared_market,
            &chainlink_history,
            &binance_history,
            config.bot.tick_interval_us(),
        ) => None,
        _ = tokio::signal::ctrl_c() => Some("SIGINT"),
        _ = sigterm.recv() => Some("SIGTERM"),
    };

    if let Some(signal) = signal_name {
        warn!("{} received. Shutting down.", signal);
    }
}

// ---------------------------------------------------------------------------
// Bot loop
// ---------------------------------------------------------------------------

async fn run_bot_loop(
    engine: &mut StrategyEngine,
    asset: &str,
    interval_minutes: u32,
    resolve_strike_price: bool,
    shared_market: &Arc<Mutex<Option<Arc<Market>>>>,
    chainlink_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
    binance_history: &Arc<Mutex<VecDeque<(f64, i64)>>>,
    tick_interval_us: u64,
) {
    wait_for_feeds(engine).await;

    loop {
        let mut market = discover_market(asset, interval_minutes).await;

        if !resolve_strike_price {
            debug!("Skipping strike resolution for {}", market.slug);
        }

        if resolve_strike_price {
            match resolve_strike_prices(chainlink_history, binance_history, &market).await {
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
                        .saturating_sub(common::time::now_ms())
                        .max(1000) as u64;
                    warn!("Skipping market {}.", market.slug);
                    sleep(Duration::from_millis(wait_ms)).await;
                    continue;
                }
            }
        }

        let price_handle = connect_poly_price_ws(shared_market, &market).await;
        trade_market(engine, &market, tick_interval_us).await;
        engine.clear_state();
        price_handle.abort();
        // Wait for another market
        let remaining_ms = market.expires_at_ms.saturating_sub(common::time::now_ms());
        if remaining_ms > 0 {
            sleep(Duration::from_millis((remaining_ms + 1000) as u64)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Market tick loop
// ---------------------------------------------------------------------------

async fn trade_market(engine: &mut StrategyEngine, market: &Market, tick_interval_us: u64) {
    debug!("Trading market {} (expires in {:.0}s)", market.slug, market.time_to_expire_ms() as f64 / 1000.0);
    let mut trade_count = 0usize;
    loop {
        if common::time::now_ms() > market.expires_at_ms + 1000 {
            break;
        }
        // Execute engine tick
        let result = engine.execute_tick().await;
        if result.traded {
            trade_count += 1;
        }

        // If Engine completed with market processing then exit
        if result.completed {
            break;
        }
        tokio::time::sleep(Duration::from_micros(tick_interval_us)).await;
    }
    debug!("Trading completed. Market {} ({} trades)", market.slug, trade_count);
}

// ---------------------------------------------------------------------------
// Telegram commands
// ---------------------------------------------------------------------------

fn bot_budget_command(budget: &Arc<Mutex<f64>>, args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        let b = *budget.lock().unwrap_or_else(|e| e.into_inner());
        return format!("{:.2} USD", b);
    }
    match args.parse::<f64>() {
        Ok(new_budget) => {
            let mut b = budget.lock().unwrap_or_else(|e| e.into_inner());
            *b = new_budget;
            format!("Budget set to {:.2} USD", new_budget)
        }
        Err(_) => "Invalid number".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_private_key(path: &str, truncate: bool) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                None
            } else {
                if truncate {
                    let _ = std::fs::write(path, "");
                    debug!("Loaded private key from {} (file truncated)", path);
                } else {
                    debug!("Loaded private key from {}", path);
                }
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
        ClobClient::new("https://clob.polymarket.com", ClobConfig::builder().build()).unwrap();

    match client_builder
        .authentication_builder(&signer)
        .signature_type(SignatureType::Proxy)
        .authenticate()
        .await
    {
        Ok(client) => Some((client, signer)),
        Err(e) => {
            error!("SDK auth failed: {e}");
            None
        }
    }
}

async fn wait_for_feeds(engine: &StrategyEngine) {
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
    debug!("All feeds ready.");
}
