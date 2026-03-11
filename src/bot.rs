mod types;
use futures_util::{SinkExt, StreamExt};
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
mod telegram;

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

async fn fetch_active_market_id(
    asset: &str,
    interval_minutes: u32,
) -> Result<(String, String, String, i64, i64, f64, f64), Box<dyn std::error::Error + Send + Sync>>
{
    println!("[BOT] Fetching active {} market...", asset.to_uppercase());

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
        println!("[BOT] Checking slug: {}", slug);

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

                let kline_url = format!(
                    "https://api.binance.com/api/v3/klines?symbol={}&interval={}&startTime={}&limit=1",
                    binance_symbol, kline_interval, ts * 1000
                );
                let strike_price_binance =
                    if let Ok(resp) = http_client.get(&kline_url).send().await {
                        if resp.status().is_success() {
                            let k_json: serde_json::Value = resp.json().await.unwrap_or_default();
                            k_json
                                .as_array()
                                .and_then(|arr| arr.first())
                                .and_then(|k| k[1].as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0)
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };

                if let (Some(tokens_str), Some(outcomes_str)) =
                    (&market.clob_token_ids, &market.outcomes)
                {
                    let tokens: Vec<String> = serde_json::from_str(tokens_str)?;
                    let outcomes: Vec<String> = serde_json::from_str(outcomes_str)?;

                    let mut up_token = String::new();
                    let mut down_token = String::new();

                    for (i, outcome) in outcomes.iter().enumerate() {
                        if outcome.eq_ignore_ascii_case("UP") || outcome.eq_ignore_ascii_case("YES")
                        {
                            if let Some(t) = tokens.get(i) {
                                up_token = t.clone();
                            }
                        } else if outcome.eq_ignore_ascii_case("DOWN")
                            || outcome.eq_ignore_ascii_case("NO")
                        {
                            if let Some(t) = tokens.get(i) {
                                down_token = t.clone();
                            }
                        }
                    }

                    if !up_token.is_empty()
                        && !down_token.is_empty()
                        && expires_at_ms > now_secs() * 1000
                    {
                        println!(
                            "[BOT] {} {}m Markets Found: UP: {} | DOWN: {}",
                            asset.to_uppercase(),
                            interval_minutes,
                            up_token,
                            down_token
                        );

                        let initial_price = market
                            .last_trade_price
                            .map(|d| d.to_string().parse::<f64>().unwrap_or(0.0))
                            .unwrap_or(0.0);
                        let initial_price = if initial_price > 0.0 {
                            initial_price
                        } else {
                            0.50
                        };

                        return Ok((
                            up_token,
                            down_token,
                            slug.clone(),
                            started_at_ms,
                            expires_at_ms,
                            strike_price_binance,
                            initial_price,
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

#[tokio::main]
async fn main() {
    let config = AppConfig::load("config.toml");
    env_logger::init();

    let trading_enabled = config.wallet.trading_enabled;
    let market_asset = config.market.asset.to_lowercase();
    let private_key = if config.wallet.private_key.is_empty() {
        None
    } else {
        Some(config.wallet.private_key.clone())
    };

    let binance_ws_url = format!(
        "wss://stream.binance.com:9443/ws/{}usdt@aggTrade",
        market_asset
    );

    if trading_enabled {
        println!("Trading ENABLED. Verifying credentials...");
        if private_key.is_none() {
            eprintln!("WARNING: trading_enabled but private_key missing!");
        }
    }

    println!(
        "Starting PolyRustBot [{}] ... Trading Enabled: {}",
        market_asset.to_uppercase(),
        trading_enabled
    );

    // --- DATA BROADCAST CHANNELS (watch) ---
    let (binance_tx, binance_rx) = watch::channel((0.0f64, 0i64));
    let (coinbase_tx, coinbase_rx) = watch::channel((0.0f64, 0i64));
    let (chainlink_tx, chainlink_rx) = watch::channel((0.0f64, 0i64));
    let (dvol_tx, dvol_rx) = watch::channel(0.0);
    let shared_market: Arc<Mutex<Option<Market>>> = Arc::new(Mutex::new(None));

    // --- BINANCE WS TASK (Primary) ---
    let binance_ws_url_clone = binance_ws_url.clone();
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse(&binance_ws_url_clone).unwrap()).await {
                Ok((ws_stream, _)) => {
                    attempts = 0;
                    println!("[BOT] Connected to Binance WS");
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(p_str) = json["p"].as_str() {
                                    if let Ok(price) = p_str.parse::<f64>() {
                                        let ts =
                                            json["T"].as_i64().unwrap_or_else(|| now_secs() * 1000);
                                        let _ = binance_tx.send((price, ts));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    eprintln!(
                        "[BOT] Binance WS Error: {}. Reconnecting in {}s...",
                        e, delay
                    );
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });

    // --- COINBASE WS TASK ---
    let coinbase_product = format!("{}-USD", market_asset.to_uppercase());
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-feed.exchange.coinbase.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    println!("[BOT] Connected to Coinbase WS");
                    let sub = serde_json::json!({
                        "type": "subscribe",
                        "product_ids": [coinbase_product],
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
                                        let _ = coinbase_tx.send((price, ts));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    eprintln!(
                        "[BOT] Coinbase WS Error: {}. Reconnecting in {}s...",
                        e, delay
                    );
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });

    // --- DERIBIT DVOL TASK ---
    let dvol_asset = market_asset.clone();
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://www.deribit.com/ws/api/v2").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    println!("[BOT] Connected to Deribit DVOL WS");
                    let subscribe_msg = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/subscribe",
                        "id": 1,
                        "params": {
                            "channels": [format!("deribit_volatility_index.{}_usd", dvol_asset)]
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
                                        let _ = dvol_tx.send(val);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    eprintln!(
                        "[BOT] Deribit WS Error: {}. Reconnecting in {}s...",
                        e, delay
                    );
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });

    // --- CHAINLINK PRICE FEED (via Polymarket live-data WS) ---
    let chainlink_history_max = (config.market.interval_minutes as usize) * 60;
    let chainlink_history: Arc<Mutex<VecDeque<(f64, i64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let chainlink_history_ws = chainlink_history.clone();
    let chainlink_asset = market_asset.clone();
    tokio::spawn(async move {
        let mut attempts = 0u32;
        loop {
            match connect_async(Url::parse("wss://ws-live-data.polymarket.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    attempts = 0;
                    println!("[BOT] Connected to Polymarket Chainlink WS");
                    let sub = serde_json::json!({
                        "action": "subscribe",
                        "subscriptions": [{
                            "topic": "crypto_prices_chainlink",
                            "type": "update",
                            "filters": format!("{{\"symbol\":\"{}/usd\"}}", chainlink_asset)
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
                                    let _ = chainlink_tx.send((price, ts));
                                    let mut hist = chainlink_history_ws
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner());
                                    hist.push_back((price, ts));
                                    if hist.len() > chainlink_history_max {
                                        hist.pop_front();
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let delay = backoff_secs(attempts);
                    eprintln!(
                        "[BOT] Chainlink WS Error: {}. Reconnecting in {}s...",
                        e, delay
                    );
                    sleep(Duration::from_secs(delay)).await;
                    attempts += 1;
                }
            }
        }
    });

    // --- TIME SYNCHRONIZATION (via SDK) ---
    let time_offset = {
        let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();
        match clob.server_time().await {
            Ok(server_ts) => {
                let local_ts = now_secs();
                let offset = server_ts - local_ts;
                println!(
                    "[BOT] Time Server: {} | Local: {} | Offset: {}s",
                    server_ts, local_ts, offset
                );
                if offset.abs() > 300 {
                    println!(
                        "[BOT] Large time skew detected! Adjusting all auth timestamps by {}s",
                        offset
                    );
                }
                offset
            }
            Err(e) => {
                eprintln!("[BOT] Failed to sync time: {}. Defaulting to 0 offset.", e);
                0
            }
        }
    };

    telegram::init(config.telegram.clone());

    let strategy = BonoStrategy::new(config.strategy.bono.clone());
    let mut engine = StrategyEngine::new(
        strategy,
        config.wallet.trading_enabled,
        market_asset.clone(),
        time_offset,
        config.engine.clone(),
        binance_rx,
        coinbase_rx,
        chainlink_rx,
        dvol_rx,
        shared_market.clone(),
    );

    if trading_enabled {
        if let Some(pk) = private_key {
            if engine.initialize_client(&pk).await {
                println!("[BOT] Polymarket SDK Authenticated.");
            }
        }
    }

    // --- GRACEFUL SHUTDOWN ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    println!("[BOT] Ready. Waiting for asset price feeds...");

    tokio::select! {
        _ = async {
            // Wait for all feeds before starting
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
            println!("[BOT] All feeds ready.");

            let interval_minutes = config.market.interval_minutes;

            // === MARKET ROTATION LOOP ===
            loop {
                // --- Discover market ---
                let (up_id, down_id, slug, started_ms, expires_ms, strike, _initial_price) = loop {
                    match fetch_active_market_id(&market_asset, interval_minutes).await {
                        Ok(result) => break result,
                        Err(e) => {
                            eprintln!("[BOT] Market discovery error: {}. Retrying...", e);
                            sleep(Duration::from_secs(5)).await;
                        }
                    }
                };

                // --- Lookup strike price from chainlink history ---
                let strike_price_chainlink = {
                    let hist = chainlink_history.lock().unwrap_or_else(|e| e.into_inner());
                    hist.iter()
                        .rev()
                        .min_by_key(|(_, ts)| (ts - started_ms).unsigned_abs())
                        .filter(|(_, ts)| (ts - started_ms).unsigned_abs() ==0)
                        .map(|(price, _)| *price)
                        .unwrap_or(0.0)
                };
                if strike_price_chainlink == 0.0 {
                    let wait_secs = (expires_ms / 1000).saturating_sub(now_secs()).max(1) as u64;
                    let interval_secs = (interval_minutes as u64) * 60;
                    eprintln!("[BOT] No strike_price price for {}.", slug);
                    if interval_secs.saturating_sub(wait_secs) > 10 {
                        sleep(Duration::from_secs(wait_secs)).await;
                    }
                    continue;
                }
                // --- Update shared market ---
                {
                    let mut guard = shared_market.lock().unwrap_or_else(|e| e.into_inner());
                    let mut m = Market::new(slug.clone(), up_id.clone(), down_id.clone(), started_ms, expires_ms, strike);
                    m.strike_price = strike_price_chainlink;
                    *guard = Some(m);
                }

                // --- Spawn Polymarket price WS task for this market ---
                let price_shared = shared_market.clone();
                let price_ids = vec![up_id.clone(), down_id.clone()];
                let price_handle = tokio::spawn(async move {
                    loop {
                        let ws_client = PolyWsClient::default();
                        let stream = match ws_client.subscribe_prices(price_ids.clone()) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("[BOT] Polymarket Chainlink WS subscribe error: {}. Retrying...", e);
                                sleep(Duration::from_secs(5)).await;
                                continue;
                            }
                        };
                        println!("[BOT] Connected to Polymarket Price WS");
                        let mut stream = Box::pin(stream);

                        while let Some(result) = stream.next().await {
                            match result {
                                Ok(price_change) => {
                                    let mut guard = price_shared.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Some(m) = guard.as_mut() {
                                        for entry in &price_change.price_changes {
                                            if let Some(side) = m.side_by_token(&entry.asset_id) {
                                                let price: f64 = entry.price.to_string().parse().unwrap_or(0.0);
                                                if price > 0.0 {
                                                    side.last_trade_price = price;
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[BOT] Polymarket Chainlink WS error: {}. Reconnecting...", e);
                                    break;
                                }
                            }
                        }
                        sleep(Duration::from_secs(2)).await;
                    }
                });

                println!("[BOT] Market active: {} | Expires: {}ms", slug, expires_ms);

                // --- Tick loop until market expires ---
                loop {
                    if now_secs() * 1000 > expires_ms + 1000 {
                        break;
                    }
                    engine.execute_tick().await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }

                // --- Cleanup: abort market-specific tasks ---
                price_handle.abort();
                engine.traded_slugs.clear();
                println!("[BOT] Market {} expired. Rotating...", slug);
            }
        } => {},
        _ = tokio::signal::ctrl_c() => {
            println!("\n[BOT] SIGINT received.");
        },
        _ = sigterm.recv() => {
            println!("\n[BOT] SIGTERM received.");
        }
    }
}
