mod adapters;
mod common;
mod domain;
mod engine;
mod ports;

use log::{debug, error, warn};
use std::sync::{Arc, Mutex};

use common::config::AppConfig;
use engine::StrategyEngine;

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
    let mode = if secrets.polygon_private_key.is_none() {
        "paper"
    } else {
        "live"
    };

    // --- Budget (shared, safe to update at runtime) ---
    let budget: Arc<Mutex<f64>> = Arc::new(Mutex::new(config.bot_initial_budget));

    debug!(
        "Starting asset={} mode={}",
        market_asset.to_uppercase(),
        mode
    );

    // --- Clock adapter (must be first — everything depends on synced time) ---
    let clock = Arc::new(adapters::clock::PolymarketClock::new());
    clock.sync().await;

    // --- Telegram adapter (inbound commands + outbound notifications) ---
    let mut cmds = adapters::telegram::Commands::new();
    let budget_ref = budget.clone();
    cmds.register("budget", "Budget", move |args| {
        bot_budget_command(&budget_ref, args)
    });
    let _tg = adapters::telegram::TelegramAdapter::spawn(
        secrets.telegram_bot_token,
        config.bot_telegram_chat_id,
        cmds.build(),
    );

    // --- Persistence adapter (Notion) ---
    let (notion, notion_api) = adapters::notion::NotionAdapter::spawn(
        secrets.notion_integration_secret,
        &config.notion_database_id,
        &config.bot_name,
    );

    // --- Polymarket resolver (background task) ---
    if let Some(api) = notion_api {
        adapters::resolver::spawn_resolver(api, config.polymarket_resolver_interval_secs);
    }

    // --- Price feed adapters ---
    let market_window_secs = (config.market_interval_minutes * 60) as i64;

    let binance_feed = Arc::new(adapters::binance::BinanceAdapter::spawn(
        &market_asset,
        clock.clone(),
        config.feeds_binance_history_ms(market_window_secs),
    ));
    let coinbase_feed = Arc::new(adapters::coinbase::CoinbaseAdapter::spawn(
        &market_asset,
        clock.clone(),
    ));
    let chainlink_feed = Arc::new(adapters::chainlink::ChainlinkAdapter::spawn(
        &market_asset,
        config.feeds_chainlink_history_ms(market_window_secs),
    ));

    // --- Market price adapter (per-market bid/ask WS) ---
    let market_price = Arc::new(adapters::polymarket_price::PolymarketPriceAdapter::new());

    // --- Exchange adapter (order signing + submission) ---
    let exchange = Arc::new(
        adapters::polymarket_order::PolymarketOrderAdapter::new(
            secrets.polygon_private_key.as_deref(),
        )
        .await,
    );

    // --- Market discovery adapter ---
    let discovery = Arc::new(adapters::polymarket_discovery::GammaDiscoveryAdapter::new(
        clock.clone(),
    ));

    // --- Strategy engine (depends only on port traits) ---
    let mut engine = StrategyEngine::new(
        &config.engine_strategy,
        binance_feed,
        coinbase_feed,
        chainlink_feed,
        market_price,
        exchange,
        discovery,
        Arc::new(notion),
        clock,
        budget,
    );

    // --- Graceful shutdown ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    let signal_name = tokio::select! {
        _ = engine.run(
            &market_asset,
            config.market_interval_minutes,
            config.market_resolve_strike_price,
            config.tick_interval_us(),
        ) => None,
        _ = tokio::signal::ctrl_c() => Some("SIGINT"),
        _ = sigterm.recv() => Some("SIGTERM"),
    };

    if let Some(signal) = signal_name {
        warn!("{} received. Shutting down.", signal);
    }
}

// ---------------------------------------------------------------------------
// Telegram commands (inbound adapter handler)
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
