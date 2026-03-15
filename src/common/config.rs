use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    // ─── Security ──────────────────────────────────────────────────────
    #[serde(default = "default_true")]
    pub truncate_secret_files: bool,

    // ─── Bot ─────────────────────────────────────────────────────────
    pub bot_name: String,
    #[serde(default = "default_key_file")]
    pub bot_polygon_private_key_file: String,
    #[serde(default)]
    pub bot_initial_budget: f64,
    #[serde(default = "default_worker_threads")]
    pub bot_worker_threads: usize,
    #[serde(default = "default_engine_ticks_per_second")]
    pub bot_engine_ticks_per_second: u32,
    #[serde(default = "default_bot_token_file")]
    pub bot_telegram_bot_token_file: String,
    #[serde(default)]
    pub bot_telegram_chat_id: i64,

    // ─── Market ──────────────────────────────────────────────────────
    #[serde(default = "default_asset")]
    pub market_asset: String,
    #[serde(default = "default_interval")]
    pub market_interval_minutes: u32,
    #[serde(default = "default_true")]
    pub market_resolve_strike_price: bool,

    // ─── Logger ──────────────────────────────────────────────────────
    #[serde(default = "default_log_level")]
    pub logger_level: String,

    // ─── Feeds ───────────────────────────────────────────────────────
    pub feeds_binance_history_secs: Option<u32>,
    pub feeds_chainlink_history_secs: Option<u32>,

    // ─── Engine ──────────────────────────────────────────────────────
    pub engine_strategy: String,
}

impl AppConfig {
    pub fn load(path: &str) -> Self {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("Failed to read config file: {}", path));
        let config: Self = toml::from_str(&content)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path, e));
        config.validate();
        config
    }

    /// Tick interval in microseconds, computed from bot_engine_ticks_per_second.
    pub fn tick_interval_us(&self) -> u64 {
        1_000_000 / self.bot_engine_ticks_per_second as u64
    }

    /// Load all secrets from their files. Returns (private_key, telegram_token).
    /// When truncate_secret_files is true, files are emptied after reading.
    pub fn load_secrets(&self) -> (Option<String>, Option<String>) {
        let truncate = self.truncate_secret_files;
        let private_key = load_secret_file(&self.bot_polygon_private_key_file, truncate);
        let tg_token = load_secret_file(&self.bot_telegram_bot_token_file, truncate);
        (private_key, tg_token)
    }

    pub fn feeds_binance_history_ms(&self, default_secs: i64) -> i64 {
        self.feeds_binance_history_secs
            .map(|s| s as i64 * 1000)
            .unwrap_or(default_secs * 1000)
    }

    pub fn feeds_chainlink_history_ms(&self, default_secs: i64) -> i64 {
        self.feeds_chainlink_history_secs
            .map(|s| s as i64 * 1000)
            .unwrap_or(default_secs * 1000)
    }

    fn validate(&self) {
        let mut errors: Vec<String> = Vec::new();

        if self.bot_name.trim().is_empty() {
            errors.push("'bot_name' is required and must not be empty".into());
        }

        if self.bot_polygon_private_key_file.trim().is_empty() {
            errors.push("bot_polygon_private_key_file must not be empty".into());
        }

        let valid_levels = ["error", "warn", "info", "debug", "trace"];
        for part in self.logger_level.split(',') {
            let level = part
                .split('=')
                .last()
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if !valid_levels.contains(&level.as_str()) {
                errors.push(format!(
                    "Invalid logger_level '{}'. Valid levels: error, warn, info, debug, trace",
                    part.trim()
                ));
            }
        }

        let valid_assets = ["btc", "eth", "sol", "xrp"];
        if !valid_assets.contains(&self.market_asset.to_lowercase().as_str()) {
            errors.push(format!(
                "Invalid market_asset: '{}'. Supported: btc, eth, sol, xrp",
                self.market_asset
            ));
        }

        let valid_intervals = [5, 15];
        if !valid_intervals.contains(&self.market_interval_minutes) {
            errors.push(format!(
                "Invalid market_interval_minutes: {}. Supported: 5, 15",
                self.market_interval_minutes
            ));
        }

        let valid_strategies = ["bono", "konzerva"];
        if !valid_strategies.contains(&self.engine_strategy.as_str()) {
            errors.push(format!(
                "Invalid engine_strategy: '{}'. Supported: bono, konzerva",
                self.engine_strategy
            ));
        }

        if self.bot_worker_threads == 0 || self.bot_worker_threads > 16 {
            errors.push(format!(
                "bot_worker_threads must be between 1 and 16, got {}",
                self.bot_worker_threads
            ));
        }

        if self.bot_engine_ticks_per_second == 0 {
            errors.push("bot_engine_ticks_per_second must be > 0".into());
        }

        if let Some(0) = self.feeds_binance_history_secs {
            errors.push("feeds_binance_history_secs must be > 0".into());
        }
        if let Some(0) = self.feeds_chainlink_history_secs {
            errors.push("feeds_chainlink_history_secs must be > 0".into());
        }

        if !errors.is_empty() {
            panic!(
                "Config validation failed:\n  - {}",
                errors.join("\n  - ")
            );
        }
    }
}

fn load_secret_file(path: &str, truncate: bool) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let value = contents.trim().to_string();
            if value.is_empty() {
                None
            } else {
                if truncate {
                    let _ = std::fs::write(path, "");
                    log::debug!("Loaded secret from {} (file truncated)", path);
                } else {
                    log::debug!("Loaded secret from {}", path);
                }
                Some(value)
            }
        }
        Err(_) => {
            log::debug!("Secret file not found: {}", path);
            None
        }
    }
}

fn default_true() -> bool { true }
fn default_bot_token_file() -> String { ".tg-token".to_string() }
fn default_key_file() -> String { ".key".to_string() }
fn default_log_level() -> String { "info".to_string() }
fn default_asset() -> String { "btc".to_string() }
fn default_interval() -> u32 { 5 }
fn default_worker_threads() -> usize { 2 }
fn default_engine_ticks_per_second() -> u32 { 1000 }
