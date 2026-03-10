use crate::config::TelegramConfig;
use std::sync::OnceLock;

static CONFIG: OnceLock<TelegramConfig> = OnceLock::new();

pub fn init(config: TelegramConfig) {
    let _ = CONFIG.set(config);
}

pub fn send_alert(message: &str) {
    let Some(config) = CONFIG.get() else { return };
    if config.bot_token.is_empty() || config.chat_id.is_empty() { return; }

    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);
    let chat_id = config.chat_id.clone();
    let message = message.to_string();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client.post(&url)
            .form(&[("chat_id", &chat_id), ("text", &message)])
            .send()
            .await;
    });
}
