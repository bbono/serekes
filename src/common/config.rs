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
    #[serde(default)]
    pub initial_budget: f64,
    #[serde(default = "default_true")]
    pub truncate_key_file: bool,
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    #[serde(default = "default_engine_ticks_per_second")]
    pub engine_ticks_per_second: u32,
}

impl BotConfig {
    /// Tick interval in microseconds, computed from engine_ticks_per_second.
    pub fn tick_interval_us(&self) -> u64 {
        1_000_000 / self.engine_ticks_per_second as u64
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketConfig {
    #[serde(default = "default_asset")]
    pub asset: String,
    #[serde(default = "default_interval")]
    pub interval_minutes: u32,
    #[serde(default = "default_true")]
    pub resolve_strike_price: bool,
}

fn default_true() -> bool {
    true
}

impl Default for MarketConfig {
    fn default() -> Self {
        Self {
            asset: default_asset(),
            interval_minutes: default_interval(),
            resolve_strike_price: true,
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

impl FeedsConfig {
    pub fn binance_history_ms(&self, default_secs: i64) -> i64 {
        self.binance_history_secs
            .map(|s| s as i64 * 1000)
            .unwrap_or(default_secs * 1000)
    }

    pub fn chainlink_history_ms(&self, default_secs: i64) -> i64 {
        self.chainlink_history_secs
            .map(|s| s as i64 * 1000)
            .unwrap_or(default_secs * 1000)
    }

    pub fn dvol_history_ms(&self, default_secs: i64) -> i64 {
        self.dvol_history_secs
            .map(|s| s as i64 * 1000)
            .unwrap_or(default_secs * 1000)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub strategy: String,
}


fn default_key_file() -> String { ".key".to_string() }
fn default_log_level() -> String { "info".to_string() }
fn default_show_timestamp() -> bool { true }
fn default_show_module() -> bool { true }
fn default_asset() -> String { "btc".to_string() }
fn default_interval() -> u32 { 5 }
fn default_worker_threads() -> usize { 2 }
fn default_engine_ticks_per_second() -> u32 { 1000 }

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


        // bot.worker_threads
        if self.bot.worker_threads == 0 || self.bot.worker_threads > 16 {
            errors.push(format!(
                "bot.worker_threads must be between 1 and 16, got {}",
                self.bot.worker_threads
            ));
        }

        // bot.engine_ticks_per_second
        if self.bot.engine_ticks_per_second == 0 {
            errors.push("bot.engine_ticks_per_second must be > 0".into());
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
