mod common;
mod engine;
mod integrations;
mod strategy;
mod types;

use log::{debug, error, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

use common::config::AppConfig;
use engine::StrategyEngine;
use integrations::{spawn_binance_ws, spawn_chainlink_ws, spawn_coinbase_ws};
use types::Market;

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let config = AppConfig::load("config.toml");
    common::logger::init(&config.bot_name, &config.logger_level);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.bot_worker_threads)
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime");

    runtime.block_on(async_main(config));
}

async fn async_main(config: AppConfig) {
    let market_asset = config.market_asset.to_lowercase();
    let secrets = config.load_secrets();
    if secrets.telegram_bot_token.is_none() {
        error!(
            "telegram_bot_token missing in secrets file '{}'",
            config.secrets_file
        );
        std::process::exit(1);
    }
    let paper_mode = secrets.polygon_private_key.is_none();
    let mode = if paper_mode { "paper" } else { "live" };

    // --- Budget (shared, safe to update at runtime) ---
    let budget: Arc<Mutex<f64>> = Arc::new(Mutex::new(config.bot_initial_budget));

    debug!(
        "Starting asset={} mode={}",
        market_asset.to_uppercase(),
        mode
    );

    // --- Time synchronization (before anything that uses now_ms) ---
    common::time::fetch_time_offset_ms().await;

    // --- Telegram ---
    let mut cmds = integrations::telegram::Commands::new();
    let budget_ref = budget.clone();
    cmds.register("budget", "Budget", move |args| {
        bot_budget_command(&budget_ref, args)
    });
    let _tg = integrations::telegram::spawn(
        secrets.telegram_bot_token,
        config.bot_telegram_chat_id,
        cmds.build(),
    );

    // --- Notion ---
    let (notion, notion_api) = integrations::notion::spawn(
        secrets.notion_integration_secret,
        &config.notion_database_id,
        &config.bot_name,
    );

    // --- Polymarket resolver ---
    if let Some(api) = notion_api {
        integrations::polymarket::spawn_resolver(api, config.polymarket_resolver_interval_secs);
    }

    // --- Data broadcast channels ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let shared_market: Arc<Mutex<Option<Arc<Market>>>> = Arc::new(Mutex::new(None));

    // --- Spawn all price feed WebSockets ---
    let market_window_secs = (config.market_interval_minutes * 60) as i64;

    let binance_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_binance_ws(
        &market_asset,
        binance_tx,
        binance_history.clone(),
        config.feeds_binance_history_ms(market_window_secs),
    );
    spawn_coinbase_ws(&market_asset, coinbase_tx);

    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    spawn_chainlink_ws(
        market_asset.clone(),
        chainlink_tx,
        chainlink_history.clone(),
        config.feeds_chainlink_history_ms(market_window_secs),
    );

    // --- Strategy engine ---
    let mut engine = StrategyEngine::new(
        paper_mode,
        &config.engine_strategy,
        binance_rx,
        coinbase_rx,
        chainlink_rx,
        shared_market,
        binance_history,
        chainlink_history,
        budget,
    );

    if let Some(pk) = secrets.polygon_private_key {
        engine.authenticate(&pk).await;
    }

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let signal_name = tokio::select! {
        _ = engine.run(
            &market_asset,
            config.market_interval_minutes,
            config.market_resolve_strike_price,
            config.tick_interval_us(),
            &notion,
        ) => None,
        _ = tokio::signal::ctrl_c() => Some("SIGINT"),
        _ = sigterm.recv() => Some("SIGTERM"),
    };

    if let Some(signal) = signal_name {
        warn!("{} received. Shutting down.", signal);
    }
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
