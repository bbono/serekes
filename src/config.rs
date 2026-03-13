use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub name: String,
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
        config.validate();
        config
    }

    fn validate(&self) {
        // name
        if self.name.trim().is_empty() {
            panic!("Config 'name' is required and must not be empty");
        }

        // log_level: validate each entry in the comma-separated filter string
        let valid_levels = ["error", "warn", "info", "debug", "trace"];
        for part in self.log_level.split(',') {
            let level = part
                .split('=')
                .last()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if !valid_levels.contains(&level.as_str()) {
                panic!(
                    "Invalid log_level '{}'. Valid levels: error, warn, info, debug, trace",
                    part.trim()
                );
            }
        }

        // market.asset
        let valid_assets = ["btc", "eth", "sol", "xrp"];
        if !valid_assets.contains(&self.market.asset.to_lowercase().as_str()) {
            panic!(
                "Invalid asset: '{}'. Supported: btc, eth, sol, xrp",
                self.market.asset
            );
        }

        // market.interval_minutes
        let valid_intervals = [5, 15];
        if !valid_intervals.contains(&self.market.interval_minutes) {
            panic!(
                "Invalid interval_minutes: {}. Supported: 5, 15",
                self.market.interval_minutes
            );
        }

        // engine.exchange_price_divergence_threshold
        if self.engine.exchange_price_divergence_threshold <= 0.0 {
            panic!(
                "exchange_price_divergence_threshold must be > 0, got {}",
                self.engine.exchange_price_divergence_threshold
            );
        }

        // engine.log_interval_secs
        if self.engine.log_interval_secs < 0.0 {
            panic!(
                "log_interval_secs must be >= 0, got {}",
                self.engine.log_interval_secs
            );
        }

        // wallet.key_file
        if self.wallet.key_file.trim().is_empty() {
            panic!("wallet.key_file must not be empty");
        }
    }
}
