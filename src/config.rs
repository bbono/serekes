use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub market: MarketConfig,
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub strategy: StrategyConfigs,
    #[serde(default)]
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub trading_enabled: bool,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            private_key: String::new(),
            trading_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    #[serde(default = "default_asset")]
    pub asset: String,
    #[serde(default = "default_interval")]
    pub interval_minutes: u32,
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self { asset: default_asset(), interval_minutes: default_interval() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct EngineConfig {
    #[serde(default = "default_budget")]
    pub budget: f64,
    #[serde(default = "default_killswitch")]
    pub exchange_price_divergence_threshold: f64,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            budget: default_budget(),
            exchange_price_divergence_threshold: default_killswitch(),
            log_interval_secs: default_log_interval(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StrategyConfigs {
    #[serde(default)]
    pub bono: crate::engine::strategies::bono::BonoStrategyConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub chat_id: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            chat_id: String::new(),
        }
    }
}

fn default_asset() -> String { "btc".to_string() }
fn default_interval() -> u32 { 5 }
fn default_budget() -> f64 { 3.0 }
fn default_killswitch() -> f64 { 50.0 }
fn default_log_interval() -> f64 { 0.5 }

impl AppConfig {
    pub fn load(path: &str) -> Self {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("Failed to read config file: {}", path));
        let config: Self = toml::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path, e));
        let valid_assets = ["btc", "eth", "sol", "xrp"];
        if !valid_assets.contains(&config.market.asset.to_lowercase().as_str()) {
            panic!(
                "Invalid asset: {}. Supported assets: btc, eth, sol, xrp",
                config.market.asset
            );
        }
        let valid_intervals = [5, 15];
        if !valid_intervals.contains(&config.market.interval_minutes) {
            panic!(
                "Invalid interval_minutes: {}. Supported intervals: 5m, 15m",
                config.market.interval_minutes
            );
        }
        config
    }
}
