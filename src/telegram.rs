use crate::config::TelegramConfig;
use std::sync::OnceLock;

static CONFIG: OnceLock<TelegramConfig> = OnceLock::new();
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub fn init(config: TelegramConfig) {
    let _ = CONFIG.set(config);
    let _ = CLIENT.set(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default(),
    );
}

pub fn send_alert(message: &str) {
    let Some(config) = CONFIG.get() else { return };
    if config.bot_token.is_empty() || config.chat_id.is_empty() { return; }

    let client = CLIENT.get().cloned().unwrap_or_default();
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);
    let chat_id = config.chat_id.clone();
    let message = message.to_string();
    tokio::spawn(async move {
        if let Err(e) = client.post(&url)
            .form(&[("chat_id", &chat_id), ("text", &message)])
            .send()
            .await
        {
            log::error!("Failed to send alert: {}", e);
        }
    });
}
