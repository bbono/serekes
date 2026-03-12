use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub market: MarketConfig,
    #[serde(default)]
    pub engine: EngineConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    #[serde(default = "default_key_file")]
    pub key_file: String,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            key_file: default_key_file(),
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
pub struct EngineConfig {
    #[serde(default = "default_killswitch")]
    pub exchange_price_divergence_threshold: f64,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: f64,
    pub binance_history_secs: Option<u32>,
    pub chainlink_history_secs: Option<u32>,
    pub dvol_history_secs: Option<u32>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            exchange_price_divergence_threshold: default_killswitch(),
            log_interval_secs: default_log_interval(),
            binance_history_secs: None,
            chainlink_history_secs: None,
            dvol_history_secs: None,
        }
    }
}

fn default_key_file() -> String { ".key".to_string() }
fn default_log_level() -> String { "info".to_string() }
fn default_asset() -> String { "btc".to_string() }
fn default_interval() -> u32 { 5 }
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
