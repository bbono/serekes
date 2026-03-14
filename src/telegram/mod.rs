pub mod commands;

use log::{debug, error, warn};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UpdateKind};
use tokio::sync::mpsc;

use crate::common::config::TelegramConfig;

/// Callback type for handling incoming commands.
/// Receives (command, args) and returns an optional reply string.
pub type CommandHandler = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// Handle for sending messages to the configured Telegram chat.
#[derive(Clone)]
pub struct Telegram {
    tx: mpsc::UnboundedSender<String>,
}

impl Telegram {
    pub fn send(&self, msg: impl Into<String>) {
        let _ = self.tx.send(escape_markdown(&msg.into()));
    }
}

/// Spawns the Telegram bot on a dedicated OS thread with its own tokio runtime,
/// fully isolated from the main engine runtime.
pub fn spawn(config: &TelegramConfig, built: commands::Built) -> Telegram {
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    if !config.is_enabled() {
        warn!("Telegram disabled (missing bot_token or chat_id)");
        return Telegram { tx };
    }

    let bot_token = config.bot_token.clone();
    let chat_id = ChatId(config.chat_id);

    std::thread::Builder::new()
        .name("telegram".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build Telegram runtime");

            rt.block_on(async move {
                let client = teloxide::net::default_reqwest_settings()
                    .timeout(std::time::Duration::from_secs(45))
                    .build()
                    .expect("Failed to build Telegram HTTP client");
                let bot = Bot::with_client(&bot_token, client);

                let bot_out = bot.clone();
                tokio::spawn(outbound_loop(bot_out, chat_id, rx));

                // Sync command menu
                if let Err(e) = bot.set_my_commands(built.menu_commands).await {
                    error!("Failed to set bot commands menu: {}", e);
                }

                let bot_in = bot.clone();
                tokio::spawn(inbound_loop(bot_in, chat_id, built.handler));

                // Keep the runtime alive
                std::future::pending::<()>().await;
            });
        })
        .expect("Failed to spawn Telegram thread");

    debug!("Telegram bot started on dedicated thread (chat_id={})", config.chat_id);

    Telegram { tx }
}

async fn outbound_loop(bot: Bot, chat_id: ChatId, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = rx.recv().await {
        for attempt in 0..3 {
            match bot.send_message(chat_id, &msg).parse_mode(ParseMode::MarkdownV2).await {
                Ok(_) => break,
                Err(e) => {
                    error!("Telegram send failed (attempt {}): {}", attempt + 1, e);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}

async fn inbound_loop(bot: Bot, owner_chat_id: ChatId, handler: CommandHandler) {
    let mut offset: i32 = 0;

    loop {
        let updates = bot
            .get_updates()
            .offset(offset)
            .timeout(30)
            .await;

        let updates = match updates {
            Ok(u) => u,
            Err(e) => {
                error!("Telegram poll error: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            offset = update.id.as_offset();

            let msg = match update.kind {
                UpdateKind::Message(ref m) => m,
                _ => continue,
            };

            if msg.chat.id != owner_chat_id {
                continue;
            }

            let text: &str = match msg.text() {
                Some(t) => t,
                None => continue,
            };

            let (cmd, args) = match text.split_once(' ') {
                Some((c, a)) => (c, a.trim()),
                None => (text, ""),
            };

            if let Some(reply) = handler(cmd, args) {
                let escaped = escape_markdown(&reply);
                if let Err(e) = bot.send_message(owner_chat_id, &escaped)
                    .parse_mode(ParseMode::MarkdownV2).await
                {
                    error!("Telegram reply failed: {}", e);
                }
            }
        }
    }
}

fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "_*[]()~`>#+-=|{}.!\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
