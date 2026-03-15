use crate::ports::NotificationPort;
use log::{debug, error, warn};
use std::collections::HashMap;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatId, ParseMode, UpdateKind};
use tokio::sync::mpsc;

/// Callback type for handling incoming commands.
pub type CommandHandler = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// Telegram adapter implementing NotificationPort.
#[derive(Clone)]
#[allow(dead_code)]
pub struct TelegramAdapter {
    tx: mpsc::UnboundedSender<String>,
}

impl TelegramAdapter {
    /// Spawns the Telegram bot on a dedicated OS thread with its own tokio runtime,
    /// fully isolated from the main engine runtime.
    pub fn spawn(bot_token: Option<String>, chat_id: i64, built: Built) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<String>();

        let bot_token = match bot_token {
            Some(token) if chat_id != 0 => token,
            _ => {
                warn!("Telegram disabled (missing bot token or chat_id)");
                return Self { tx };
            }
        };

        let chat_id = ChatId(chat_id);

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

        debug!("Telegram bot started on dedicated thread (chat_id={})", chat_id);

        Self { tx }
    }
}

impl NotificationPort for TelegramAdapter {
    fn send(&self, msg: String) {
        let _ = self.tx.send(escape_markdown(&msg));
    }
}

async fn outbound_loop(bot: Bot, chat_id: ChatId, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = rx.recv().await {
        for attempt in 0..3 {
            match bot
                .send_message(chat_id, &msg)
                .parse_mode(ParseMode::MarkdownV2)
                .await
            {
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
        let updates = bot.get_updates().offset(offset).timeout(30).await;

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
                if let Err(e) = bot
                    .send_message(owner_chat_id, &escaped)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await
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

// ---------------------------------------------------------------------------
// Commands builder (inbound adapter helper)
// ---------------------------------------------------------------------------

type Handler = Box<dyn Fn(&str) -> String + Send + Sync>;

pub struct Commands {
    handlers: HashMap<String, Handler>,
    menu_commands: Vec<BotCommand>,
}

impl Commands {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            menu_commands: Vec::new(),
        }
    }

    pub fn register(
        &mut self,
        cmd: &str,
        description: &str,
        handler: impl Fn(&str) -> String + Send + Sync + 'static,
    ) {
        let name = cmd.strip_prefix('/').unwrap_or(cmd);
        let key = format!("/{}", name);
        self.handlers.insert(key, Box::new(handler));
        self.menu_commands
            .push(BotCommand::new(name.to_string(), description));
    }

    pub fn build(self) -> Built {
        let handlers = Arc::new(self.handlers);
        let handler: CommandHandler = Arc::new(move |cmd: &str, args: &str| {
            handlers.get(cmd).map(|h| h(args))
        });
        Built {
            handler,
            menu_commands: self.menu_commands,
        }
    }
}

pub struct Built {
    pub handler: CommandHandler,
    pub menu_commands: Vec<BotCommand>,
}
