use ethers::prelude::*;
mod types;
use types::{Market, TokenDirection};
use chrono::{Utc, Timelike};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::fs;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::gamma;

mod strategy;
mod state;
mod atr;
mod velocity;
mod config;
mod telegram;

use crate::strategy::StrategyEngine;
use crate::config::AppConfig;
use tokio::sync::watch;

async fn fetch_active_market_id(asset: &str, interval_minutes: u32) -> Result<(String, String, i64, f64, f64), Box<dyn std::error::Error + Send + Sync>> {
    println!("Fetching active {} market ID...", asset.to_uppercase());
    let now = Utc::now();
    let minute = now.minute();

    let binance_symbol = format!("{}USDT", asset.to_uppercase());
    let bucket_size_min = interval_minutes;
    let interval_secs = (interval_minutes as u64) * 60;
    let kline_interval = format!("{}m", interval_minutes);

    let current_bucket_minute = (minute / bucket_size_min) * bucket_size_min;
    let current_bucket_start = now
        .with_minute(current_bucket_minute)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();

    let timestamps_to_check = vec![
        current_bucket_start.timestamp(),
        current_bucket_start.timestamp() + interval_secs as i64,
    ];

    let gamma_client = gamma::Client::default();
    let http_client = reqwest::Client::new();

    for ts in timestamps_to_check {
        let slug = format!("{}-updown-{}-{}", asset, kline_interval, ts);
        println!("Checking slug: {}", slug);

        let request = gamma::types::request::EventBySlugRequest::builder().slug(&slug).build();
        let event = match gamma_client.event_by_slug(&request).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        if let Some(markets) = event.markets {
            if let Some(market) = markets.first() {
                let expiration = market.end_date
                    .map(|d| d.timestamp())
                    .unwrap_or(0);

                let kline_url = format!(
                    "https://api.binance.com/api/v3/klines?symbol={}&interval={}&startTime={}&limit=1",
                    binance_symbol, kline_interval, ts * 1000
                );
                let strike_price = if let Ok(resp) = http_client.get(&kline_url).send().await {
                    if resp.status().is_success() {
                        let k_json: serde_json::Value = resp.json().await.unwrap_or_default();
                        k_json.as_array()
                            .and_then(|arr| arr.first())
                            .and_then(|k| k[1].as_str())
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.0)
                    } else { 0.0 }
                } else { 0.0 };

                if let (Some(tokens_str), Some(outcomes_str)) = (&market.clob_token_ids, &market.outcomes) {
                    let tokens: Vec<String> = serde_json::from_str(tokens_str)?;
                    let outcomes: Vec<String> = serde_json::from_str(outcomes_str)?;

                    let mut up_token = String::new();
                    let mut down_token = String::new();

                    for (i, outcome) in outcomes.iter().enumerate() {
                        if outcome.eq_ignore_ascii_case("UP") || outcome.eq_ignore_ascii_case("YES") {
                            if let Some(t) = tokens.get(i) { up_token = t.clone(); }
                        } else if outcome.eq_ignore_ascii_case("DOWN") || outcome.eq_ignore_ascii_case("NO") {
                            if let Some(t) = tokens.get(i) { down_token = t.clone(); }
                        }
                    }

                    if !up_token.is_empty() && !down_token.is_empty() && expiration > Utc::now().timestamp() {
                        println!("[SUCCESS] {} {}m Markets Found: UP: {} | DOWN: {}",
                            asset.to_uppercase(), bucket_size_min, up_token, down_token);

                        let initial_price = market.last_trade_price
                            .map(|d| d.to_string().parse::<f64>().unwrap_or(0.0))
                            .unwrap_or(0.0);
                        let initial_price = if initial_price > 0.0 { initial_price } else { 0.50 };

                        return Ok((up_token, down_token, expiration, strike_price, initial_price));
                    }
                }
            }
        }
    }

    Err(format!("No active {} {}m market found", asset.to_uppercase(), bucket_size_min).into())
}

struct LockGuard;
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(".bot.lock");
    }
}

#[tokio::main]
async fn main() {
    let config = AppConfig::load("config.toml");
    env_logger::init();

    let trading_enabled = config.wallet.trading_enabled;
    let market_asset = config.market.asset.to_lowercase();
    let private_key = if config.wallet.private_key.is_empty() { None } else { Some(config.wallet.private_key.clone()) };

    // --- SINGLETON LOCK GUARD ---
    let lock_file = ".bot.lock";
    if fs::metadata(lock_file).is_ok() {
        eprintln!("[CRITICAL] LOCK FILE DETECTED ({}). ANOTHER BOT IS RUNNING OR CRASHED.", lock_file);
        eprintln!("To force start: rm {}", lock_file);
        std::process::exit(1);
    }
    let _ = fs::write(lock_file, Utc::now().to_rfc3339());
    let _lock_guard = LockGuard;

    let binance_ws_url = format!("wss://stream.binance.com:9443/ws/{}usdt@aggTrade", market_asset);

    if trading_enabled {
        println!("Trading ENABLED. Verifying credentials...");
        if private_key.is_none() {
             eprintln!("WARNING: trading_enabled but private_key missing!");
        }
    }

    println!("Starting PolyRustBot [{}] ... Trading Enabled: {}", market_asset.to_uppercase(), trading_enabled);

    // --- ZOMBIE KILLER: Kill any other running instances of the bot ---
    if trading_enabled {
        println!("[SAFETY] Ensuring no duplicate bots are running (pkill)...");
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg("pkill -9 poly_rust_bot")
            .spawn();
        sleep(Duration::from_millis(500)).await;
    }

    // --- DATA BROADCAST CHANNELS (watch) ---
    let (binance_tx, binance_rx) = watch::channel(0.0);
    let (coinbase_tx, coinbase_rx) = watch::channel(0.0);
    let (dvol_tx, dvol_rx) = watch::channel(0.0);
    let (polymarket_tx, polymarket_rx) = watch::channel(HashMap::<String, Market>::new());
    let (market_id_tx, market_id_rx) = watch::channel(None);

    // --- BINANCE WS TASK (Primary) ---
    let binance_ws_url_clone = binance_ws_url.clone();
    tokio::spawn(async move {
        loop {
            match connect_async(Url::parse(&binance_ws_url_clone).unwrap()).await {
                Ok((ws_stream, _)) => {
                    println!("[FEED] Connected to Binance WS");
                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(p_str) = json["p"].as_str() {
                                    if let Ok(price) = p_str.parse::<f64>() {
                                        let _ = binance_tx.send(price);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[FEED] Binance WS Error: {}. Reconnecting...", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    // --- COINBASE WS TASK (Whale Alarm) ---
    let coinbase_product = format!("{}-USD", market_asset.to_uppercase());
    tokio::spawn(async move {
        loop {
            match connect_async(Url::parse("wss://ws-feed.exchange.coinbase.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    println!("[FEED] Connected to Coinbase WS");
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
                                        let _ = coinbase_tx.send(price);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[FEED] Coinbase WS Error: {}. Reconnecting...", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    // --- DERIBIT DVOL TASK (Macro Filter) ---
    let dvol_asset = market_asset.clone();
    tokio::spawn(async move {
        loop {
            match connect_async(Url::parse("wss://www.deribit.com/ws/api/v2").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    println!("[FEED] Connected to Deribit DVOL WS");
                    let subscribe_msg = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "public/subscribe",
                        "id": 1,
                        "params": {
                            "channels": [format!("deribit_volatility_index.{}_usd", dvol_asset)]
                        }
                    });
                    let _ = ws_stream.send(Message::Text(subscribe_msg.to_string())).await;

                    let (_, mut read) = ws_stream.split();
                    while let Some(msg) = read.next().await {
                        if let Ok(Message::Text(text)) = msg {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                if json["method"] == "subscription" {
                                    if let Some(val) = json["params"]["data"]["volatility"].as_f64() {
                                        let _ = dvol_tx.send(val);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[FEED] Deribit WS Error: {}. Reconnecting...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    // --- POLYMARKET WS TASK (Orderbook) - SDK ---
    let market_asset_clone = market_asset.clone();
    let interval_minutes = config.market.interval_minutes;
    tokio::spawn(async move {
        loop {
            let (up_id, down_id, exp, strike, initial_price);
            match fetch_active_market_id(&market_asset_clone, interval_minutes).await {
                Ok((u, d, e, s, p)) => {
                    up_id = u; down_id = d; exp = e; strike = s; initial_price = p;
                    let _ = market_id_tx.send(Some(up_id.clone()));
                }
                Err(e) => {
                    eprintln!("[FEED] Polymarket meta error: {}. Retrying...", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }

            let mut local_markets = HashMap::new();
            let mut m_up = Market::new(up_id.clone(), TokenDirection::Up, exp, strike);
            m_up.last_price = initial_price;
            local_markets.insert(up_id.clone(), m_up);

            let mut m_down = Market::new(down_id.clone(), TokenDirection::Down, exp, strike);
            m_down.last_price = 1.0 - initial_price;
            local_markets.insert(down_id.clone(), m_down);

            let _ = polymarket_tx.send(local_markets.clone());

            let ws_client = PolyWsClient::default();
            let stream = match ws_client.subscribe_orderbook(vec![up_id.clone(), down_id.clone()]) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[FEED] Polymarket WS subscribe error: {}. Retrying...", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            println!("[FEED] Connected to Polymarket WS (SDK)");
            let mut stream = Box::pin(stream);

            loop {
                match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
                    Ok(Some(Ok(book))) => {
                        if Utc::now().timestamp() > exp + 2 { break; }

                        if let Some(m) = local_markets.get_mut(&book.asset_id) {
                            let bids: Vec<(String, String)> = book.bids.iter()
                                .map(|l| (l.price.to_string(), l.size.to_string()))
                                .collect();
                            let asks: Vec<(String, String)> = book.asks.iter()
                                .map(|l| (l.price.to_string(), l.size.to_string()))
                                .collect();
                            m.orderbook.update(
                                if bids.is_empty() { None } else { Some(bids) },
                                if asks.is_empty() { None } else { Some(asks) },
                            );
                            if let (Some(bid), Some(ask)) = (m.orderbook.best_bid(), m.orderbook.best_ask()) {
                                m.last_price = (bid + ask) / 2.0;
                            }
                            let _ = polymarket_tx.send(local_markets.clone());
                        }
                    }
                    Ok(Some(Err(e))) => {
                        eprintln!("[FEED] Polymarket WS error: {}. Reconnecting...", e);
                        break;
                    }
                    Ok(None) => {
                        eprintln!("[FEED] Polymarket WS stream ended. Reconnecting...");
                        break;
                    }
                    Err(_) => {
                        if Utc::now().timestamp() > exp + 2 { break; }
                    }
                }
            }
        }
    });

    // --- TIME SYNCHRONIZATION (via SDK) ---
    let time_offset = {
        let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::default()).unwrap();
        match clob.server_time().await {
            Ok(server_ts) => {
                let local_ts = Utc::now().timestamp();
                let offset = server_ts - local_ts;
                println!("[TIME] Server: {} | Local: {} | Offset: {}s", server_ts, local_ts, offset);
                if offset.abs() > 300 {
                    println!("[WARN] Large time skew detected! Adjusting all auth timestamps by {}s", offset);
                }
                offset
            },
            Err(e) => {
                eprintln!("[WARN] Failed to sync time: {}. Defaulting to 0 offset.", e);
                0
            }
        }
    };

    // --- INITIALIZE REAL BALANCE IF TRADING ---
    let mut initial_bal = 0.0;
    if trading_enabled {
        if let Some(pk) = &private_key {
            if let Ok(provider) = Provider::<Http>::try_from(config.wallet.polygon_rpc_url.as_str()) {
                if let Ok(wallet) = pk.parse::<LocalWallet>() {
                    let client_middleware = SignerMiddleware::new(provider, wallet.with_chain_id(137u64));
                    let usdc_address = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174".parse::<Address>().unwrap();

                    let target_addr = client_middleware.address();

                    let data = [
                        0x70, 0xa0, 0x82, 0x31,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    ].to_vec();

                    let mut call_data = data;
                    call_data.extend_from_slice(target_addr.as_bytes());

                    let tx = TransactionRequest::new()
                        .to(usdc_address)
                        .data(call_data);

                    if let Ok(res) = client_middleware.call(&tx.into(), None).await {
                        let bal = U256::from_big_endian(&res);
                        initial_bal = bal.as_u128() as f64 / 1_000_000.0;
                        println!("[AUTH] Synced Real Balance: ${:.2}", initial_bal);
                    }
                }
            }
        }
    }

    telegram::init(config.telegram.clone());

    let mut strategy = StrategyEngine::new(
        config.wallet.trading_enabled,
        market_asset.clone(),
        time_offset,
        config.wallet.polygon_rpc_url.clone(),
        config.strategy.clone(),
        binance_rx,
        coinbase_rx,
        dvol_rx,
        polymarket_rx,
        market_id_rx,
    ).await;

    if trading_enabled {
        strategy.state.simulated_balance = initial_bal;
        println!("[SAFETY] Live Mode detected. Overriding memory with real wallet balance: ${:.2}", initial_bal);
    }

    if trading_enabled {
        if let Some(pk) = private_key {
            if strategy.initialize_client(&pk).await {
                println!("[AUTH] Polymarket SDK Authenticated.");
            }
        }
    }

    // --- GRACEFUL SHUTDOWN ---
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    println!("[SNIPER] High-Frequency Loop Active. Priority: 1ms.");

    tokio::select! {
        _ = async {
            loop {
                let b_price = *strategy.binance_rx.borrow();
                let c_price = *strategy.coinbase_rx.borrow();
                strategy.execute_tick(b_price, c_price).await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        } => {},
        _ = tokio::signal::ctrl_c() => {
            println!("\n[SHUTDOWN] SIGINT received.");
            strategy.state.save();
        },
        _ = sigterm.recv() => {
            println!("\n[SHUTDOWN] SIGTERM received.");
            strategy.state.save();
        }
    }

    let _ = fs::remove_file(".bot.lock");
}
