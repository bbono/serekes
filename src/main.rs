mod types;
use futures_util::{SinkExt, StreamExt};
use polymarket_client_sdk::clob::ws::Client as PolyWsClient;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::gamma;
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Raw JSON-RPC eth_call. Returns USDC balance (6 decimals) as f64.
pub async fn rpc_eth_call(rpc_url: &str, to: &str, data: &str) -> Option<f64> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
        "id": 1
    });
    let resp = client.post(rpc_url).json(&body).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let hex_str = json["result"].as_str()?;
    let hex_str = hex_str.trim_start_matches("0x");
    let val = u128::from_str_radix(hex_str, 16).ok()?;
    Some(val as f64 / 1_000_000.0)
}

async fn fetch_active_market_id(
    asset: &str,
    interval_minutes: u32,
) -> Result<(String, String, String, i64, i64, f64, f64), Box<dyn std::error::Error + Send + Sync>> {
    println!("Fetching active {} market ID...", asset.to_uppercase());

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
    let http_client = reqwest::Client::new();

    for ts in timestamps_to_check {
        let slug = format!("{}-updown-{}-{}", asset, kline_interval, ts);
        println!("Checking slug: {}", slug);

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
                let strike_price = if let Ok(resp) = http_client.get(&kline_url).send().await {
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

                    if !up_token.is_empty() && !down_token.is_empty() && expires_at_ms > now_secs() * 1000 {
                        println!(
                            "[SUCCESS] {} {}m Markets Found: UP: {} | DOWN: {}",
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
                            strike_price,
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
                                        let ts = json["T"].as_i64().unwrap_or_else(|| now_secs() * 1000);
                                        let _ = binance_tx.send((price, ts));
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

    // --- COINBASE WS TASK ---
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
                                        let ts = now_secs() * 1000;
                                        let _ = coinbase_tx.send((price, ts));
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

    // --- DERIBIT DVOL TASK ---
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
                    eprintln!("[FEED] Deribit WS Error: {}. Reconnecting...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    // --- CHAINLINK PRICE FEED (via Polymarket live-data WS) ---
    let chainlink_asset = market_asset.clone();
    tokio::spawn(async move {
        loop {
            match connect_async(Url::parse("wss://ws-live-data.polymarket.com").unwrap()).await {
                Ok((mut ws_stream, _)) => {
                    println!("[FEED] Connected to Polymarket Chainlink WS");
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
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[FEED] Chainlink WS Error: {}. Reconnecting...", e);
                    sleep(Duration::from_secs(5)).await;
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
                    "[TIME] Server: {} | Local: {} | Offset: {}s",
                    server_ts, local_ts, offset
                );
                if offset.abs() > 300 {
                    println!(
                        "[WARN] Large time skew detected! Adjusting all auth timestamps by {}s",
                        offset
                    );
                }
                offset
            }
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
            use alloy_signer_local::LocalSigner;
            use std::str::FromStr;
            if let Ok(signer) = LocalSigner::from_str(pk) {
                let addr = alloy_signer::Signer::address(&signer);
                let addr_hex = format!("{:x}", addr);
                if let Some(bal) = rpc_eth_call(
                    &config.wallet.polygon_rpc_url,
                    "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
                    &format!("0x70a08231000000000000000000000000{}", addr_hex),
                )
                .await
                {
                    initial_bal = bal;
                    println!("[AUTH] Synced Real Balance: ${:.2}", initial_bal);
                }
            }
        }
    }

    telegram::init(config.telegram.clone());

    let starting_balance = if trading_enabled { initial_bal } else { 3.0 };
    let strategy = BonoStrategy::new(config.strategy.bono.clone());
    let mut engine = StrategyEngine::new(
        strategy,
        config.wallet.trading_enabled,
        market_asset.clone(),
        time_offset,
        config.wallet.polygon_rpc_url.clone(),
        config.engine.clone(),
        starting_balance,
        binance_rx,
        coinbase_rx,
        chainlink_rx,
        dvol_rx,
        shared_market.clone(),
    );

    if trading_enabled {
        if let Some(pk) = private_key {
            if engine.initialize_client(&pk).await {
                println!("[AUTH] Polymarket SDK Authenticated.");
            }
        }
    }

    // --- GRACEFUL SHUTDOWN ---
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    println!("[BOT] Ready. Waiting for Binance feed...");

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
            println!("[FEED] All feeds ready.");

            let interval_minutes = config.market.interval_minutes;

            // === MARKET ROTATION LOOP ===
            loop {
                // --- Discover market ---
                let (up_id, down_id, slug, started_ms, expires_ms, strike, initial_price) = loop {
                    match fetch_active_market_id(&market_asset, interval_minutes).await {
                        Ok(result) => break result,
                        Err(e) => {
                            eprintln!("[MARKET] Discovery error: {}. Retrying...", e);
                            sleep(Duration::from_secs(5)).await;
                        }
                    }
                };

                // --- Update shared market ---
                {
                    let mut guard = shared_market.lock().unwrap();
                    let mut m = Market::new(slug.clone(), up_id.clone(), down_id.clone(), started_ms, expires_ms, strike);
                    m.up.orderbook_mid_price = initial_price;
                    m.down.orderbook_mid_price = 1.0 - initial_price;
                    *guard = Some(m);
                }

                // --- Spawn Polymarket WS tasks for this market ---
                let ob_shared = shared_market.clone();
                let ob_up = up_id.clone();
                let ob_down = down_id.clone();
                let ob_handle = tokio::spawn(async move {
                    loop {
                        let ws_client = PolyWsClient::default();
                        let stream = match ws_client.subscribe_orderbook(vec![ob_up.clone(), ob_down.clone()]) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("[FEED] Polymarket WS subscribe error: {}. Retrying...", e);
                                sleep(Duration::from_secs(5)).await;
                                continue;
                            }
                        };

                        println!("[FEED] Connected to Polymarket Orderbook WS");
                        let mut stream = Box::pin(stream);

                        while let Some(result) = stream.next().await {
                            match result {
                                Ok(book) => {
                                    let mut guard = ob_shared.lock().unwrap();
                                    if let Some(m) = guard.as_mut() {
                                        if let Some(side) = m.side_by_token(&book.asset_id) {
                                            let bids: Vec<(String, String)> = book
                                                .bids
                                                .iter()
                                                .map(|l| (l.price.to_string(), l.size.to_string()))
                                                .collect();
                                            let asks: Vec<(String, String)> = book
                                                .asks
                                                .iter()
                                                .map(|l| (l.price.to_string(), l.size.to_string()))
                                                .collect();
                                            side.orderbook.update(
                                                if bids.is_empty() { None } else { Some(bids) },
                                                if asks.is_empty() { None } else { Some(asks) },
                                            );
                                            if let (Some(bid), Some(ask)) =
                                                (side.orderbook.best_bid(), side.orderbook.best_ask())
                                            {
                                                side.orderbook_mid_price = (bid + ask) / 2.0;
                                                side.best_bid = bid;
                                                side.best_ask = ask;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[FEED] Polymarket WS error: {}. Reconnecting...", e);
                                    break;
                                }
                            }
                        }
                        sleep(Duration::from_secs(2)).await;
                    }
                });

                let price_shared = shared_market.clone();
                let price_ids = vec![up_id.clone(), down_id.clone()];
                let price_handle = tokio::spawn(async move {
                    loop {
                        let ws_client = PolyWsClient::default();
                        let stream = match ws_client.subscribe_prices(price_ids.clone()) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("[PRICE] Subscribe error: {}. Retrying...", e);
                                sleep(Duration::from_secs(5)).await;
                                continue;
                            }
                        };
                        println!("[FEED] Connected to Polymarket Price WS");
                        let mut stream = Box::pin(stream);

                        while let Some(result) = stream.next().await {
                            match result {
                                Ok(price_change) => {
                                    let mut guard = price_shared.lock().unwrap();
                                    if let Some(m) = guard.as_mut() {
                                        for entry in &price_change.price_changes {
                                            if let Some(side) = m.side_by_token(&entry.asset_id) {
                                                let price: f64 = entry.price.to_string().parse().unwrap_or(0.0);
                                                if price > 0.0 {
                                                    side.last_trade_price = price;
                                                }
                                                if let Some(bid) = entry.best_bid {
                                                    let b: f64 = bid.to_string().parse().unwrap_or(0.0);
                                                    if b > 0.0 {
                                                        side.best_bid = b;
                                                    }
                                                }
                                                if let Some(ask) = entry.best_ask {
                                                    let a: f64 = ask.to_string().parse().unwrap_or(0.0);
                                                    if a > 0.0 {
                                                        side.best_ask = a;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[PRICE] WS error: {}. Reconnecting...", e);
                                    break;
                                }
                            }
                        }
                        sleep(Duration::from_secs(2)).await;
                    }
                });

                println!("[MARKET] Active: {} | Expires: {}ms", slug, expires_ms);

                // --- Tick loop until market expires ---
                loop {
                    if now_secs() * 1000 > expires_ms + 1000 {
                        break;
                    }
                    let b = *engine.binance_rx.borrow();
                    let c = *engine.coinbase_rx.borrow();
                    let cl = *engine.chainlink_rx.borrow();
                    engine.execute_tick(b, c, cl).await;
                    tokio::task::yield_now().await;
                }

                // --- Cleanup: abort market-specific tasks ---
                ob_handle.abort();
                price_handle.abort();
                println!("[MARKET] {} expired. Rotating...", slug);
            }
        } => {},
        _ = tokio::signal::ctrl_c() => {
            println!("\n[SHUTDOWN] SIGINT received.");
        },
        _ = sigterm.recv() => {
            println!("\n[SHUTDOWN] SIGTERM received.");
        }
    }

}
