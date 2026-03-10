use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub wallet: WalletConfig,
    #[serde(default)]
    pub market: MarketConfig,
    #[serde(default)]
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub trading_enabled: bool,
    #[serde(default = "default_rpc_url")]
    pub polygon_rpc_url: String,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            private_key: String::new(),
            trading_enabled: false,
            polygon_rpc_url: default_rpc_url(),
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
pub struct StrategyConfig {
    #[serde(default = "default_killswitch")]
    pub killswitch_threshold: f64,
    #[serde(default = "default_max_spread")]
    pub max_spread: f64,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: i64,
    #[serde(default = "default_divergence_exit")]
    pub divergence_exit_pct: f64,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            killswitch_threshold: default_killswitch(),
            max_spread: default_max_spread(),
            log_interval_secs: default_log_interval(),
            divergence_exit_pct: default_divergence_exit(),
        }
    }
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

fn default_rpc_url() -> String { "https://polygon-rpc.com".to_string() }
fn default_asset() -> String { "btc".to_string() }
fn default_interval() -> u32 { 5 }
fn default_killswitch() -> f64 { 50.0 }
fn default_max_spread() -> f64 { 0.05 }
fn default_log_interval() -> i64 { 1 }
fn default_divergence_exit() -> f64 { 0.15 }

impl AppConfig {
    pub fn load(path: &str) -> Self {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("Failed to read config file: {}", path));
        toml::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path, e))
    }
}
