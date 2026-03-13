use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub bot: BotConfig,
    #[serde(default)]
    pub market: MarketConfig,
    #[serde(default)]
    pub logger: LoggerConfig,
    #[serde(default)]
    pub feeds: FeedsConfig,
    pub engine: EngineConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggerConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_show_timestamp")]
    pub show_timestamp: bool,
    #[serde(default = "default_show_module")]
    pub show_module: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            show_timestamp: default_show_timestamp(),
            show_module: default_show_module(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedsConfig {
    pub binance_history_secs: Option<u32>,
    pub chainlink_history_secs: Option<u32>,
    pub dvol_history_secs: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub name: String,
    #[serde(default = "default_key_file")]
    pub key_file: String,
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
        Self {
            asset: default_asset(),
            interval_minutes: default_interval(),
        }
    }
}

impl Default for FeedsConfig {
    fn default() -> Self {
        Self {
            binance_history_secs: None,
            chainlink_history_secs: None,
            dvol_history_secs: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub strategy: String,
    #[serde(default = "default_killswitch")]
    pub exchange_price_divergence_threshold: f64,
    #[serde(default = "default_log_interval")]
    pub log_interval_secs: f64,
}


fn default_key_file() -> String { ".key".to_string() }
fn default_log_level() -> String { "info".to_string() }
fn default_show_timestamp() -> bool { true }
fn default_show_module() -> bool { true }
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

        // bot.name (required, no default)
        if self.bot.name.trim().is_empty() {
            errors.push("'bot.name' is required and must not be empty".into());
        }

        // bot.key_file
        if self.bot.key_file.trim().is_empty() {
            errors.push("bot.key_file must not be empty".into());
        }

        // logger.level
        let valid_levels = ["error", "warn", "info", "debug", "trace"];
        for part in self.logger.level.split(',') {
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

        // market.asset
        let valid_assets = ["btc", "eth", "sol", "xrp"];
        if !valid_assets.contains(&self.market.asset.to_lowercase().as_str()) {
            errors.push(format!(
                "Invalid market.asset: '{}'. Supported: btc, eth, sol, xrp",
                self.market.asset
            ));
        }

        // market.interval_minutes
        let valid_intervals = [5, 15];
        if !valid_intervals.contains(&self.market.interval_minutes) {
            errors.push(format!(
                "Invalid market.interval_minutes: {}. Supported: 5, 15",
                self.market.interval_minutes
            ));
        }

        // engine.strategy (required, no default)
        let valid_strategies = ["bono", "konzerva"];
        if !valid_strategies.contains(&self.engine.strategy.as_str()) {
            errors.push(format!(
                "Invalid engine.strategy: '{}'. Supported: bono, konzerva",
                self.engine.strategy
            ));
        }

        // engine.exchange_price_divergence_threshold
        if self.engine.exchange_price_divergence_threshold <= 0.0 {
            errors.push(format!(
                "engine.exchange_price_divergence_threshold must be > 0, got {}",
                self.engine.exchange_price_divergence_threshold
            ));
        }

        // engine.log_interval_secs
        if self.engine.log_interval_secs < 0.0 {
            errors.push(format!(
                "engine.log_interval_secs must be >= 0, got {}",
                self.engine.log_interval_secs
            ));
        }

        // feeds.*_history_secs
        if let Some(0) = self.feeds.binance_history_secs {
            errors.push("feeds.binance_history_secs must be > 0".into());
        }
        if let Some(0) = self.feeds.chainlink_history_secs {
            errors.push("feeds.chainlink_history_secs must be > 0".into());
        }
        if let Some(0) = self.feeds.dvol_history_secs {
            errors.push("feeds.dvol_history_secs must be > 0".into());
        }

        if !errors.is_empty() {
            panic!(
                "Config validation failed:\n  - {}",
                errors.join("\n  - ")
            );
        }
    }
}
