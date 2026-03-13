use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub strategy: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_key_file")]
    pub key_file: String,
    #[serde(default = "default_asset")]
    pub asset: String,
    #[serde(default = "default_interval")]
    pub interval_minutes: u32,
    #[serde(default = "default_killswitch")]
    pub exchange_price_divergence_threshold: f64,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: f64,
    pub binance_history_secs: Option<u32>,
    pub chainlink_history_secs: Option<u32>,
    pub dvol_history_secs: Option<u32>,
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
        let mut errors: Vec<String> = Vec::new();

        // name (required, no default)
        if self.name.trim().is_empty() {
            errors.push("'name' is required and must not be empty".into());
        }

        // strategy (required, no default)
        let valid_strategies = ["bono", "konzerva"];
        if !valid_strategies.contains(&self.strategy.as_str()) {
            errors.push(format!(
                "Invalid strategy: '{}'. Supported: bono, konzerva",
                self.strategy
            ));
        }

        // log_level
        let valid_levels = ["error", "warn", "info", "debug", "trace"];
        for part in self.log_level.split(',') {
            let level = part
                .split('=')
                .last()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if !valid_levels.contains(&level.as_str()) {
                errors.push(format!(
                    "Invalid log_level '{}'. Valid levels: error, warn, info, debug, trace",
                    part.trim()
                ));
            }
        }

        // key_file
        if self.key_file.trim().is_empty() {
            errors.push("key_file must not be empty".into());
        }

        // asset
        let valid_assets = ["btc", "eth", "sol", "xrp"];
        if !valid_assets.contains(&self.asset.to_lowercase().as_str()) {
            errors.push(format!(
                "Invalid asset: '{}'. Supported: btc, eth, sol, xrp",
                self.asset
            ));
        }

        // interval_minutes
        let valid_intervals = [5, 15];
        if !valid_intervals.contains(&self.interval_minutes) {
            errors.push(format!(
                "Invalid interval_minutes: {}. Supported: 5, 15",
                self.interval_minutes
            ));
        }

        // exchange_price_divergence_threshold
        if self.exchange_price_divergence_threshold <= 0.0 {
            errors.push(format!(
                "exchange_price_divergence_threshold must be > 0, got {}",
                self.exchange_price_divergence_threshold
            ));
        }

        // log_interval_secs
        if self.log_interval_secs < 0.0 {
            errors.push(format!(
                "log_interval_secs must be >= 0, got {}",
                self.log_interval_secs
            ));
        }

        // *_history_secs
        if let Some(0) = self.binance_history_secs {
            errors.push("binance_history_secs must be > 0".into());
        }
        if let Some(0) = self.chainlink_history_secs {
            errors.push("chainlink_history_secs must be > 0".into());
        }
        if let Some(0) = self.dvol_history_secs {
            errors.push("dvol_history_secs must be > 0".into());
        }

        if !errors.is_empty() {
            panic!(
                "Config validation failed:\n  - {}",
                errors.join("\n  - ")
            );
        }
    }
}
