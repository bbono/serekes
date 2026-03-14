use std::collections::HashMap;
use std::sync::Arc;
use teloxide::types::BotCommand;
use super::CommandHandler;

type Handler = Box<dyn Fn(&str) -> String + Send + Sync>;

pub struct Commands {
    prefix: String,
    handlers: HashMap<String, Handler>,
    menu_commands: Vec<BotCommand>,
}

impl Commands {
    pub fn new(bot_name: &str) -> Self {
        Self {
            prefix: bot_name.to_lowercase(),
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
        let prefixed = format!("/{}_{}", self.prefix, name);
        let menu_name = format!("{}_{}", self.prefix, name);
        self.handlers.insert(prefixed, Box::new(handler));
        self.menu_commands.push(BotCommand::new(menu_name, description));
    }

    pub fn build(self) -> Built {
        let handlers = Arc::new(self.handlers);
        let handler: CommandHandler = Arc::new(move |cmd: &str, args: &str| {
            handlers.get(cmd).map(|h| h(args))
        });
        Built {
            handler,
            menu_commands: self.menu_commands,
            prefix: self.prefix,
        }
    }
}

pub struct Built {
    pub handler: CommandHandler,
    pub menu_commands: Vec<BotCommand>,
    pub prefix: String,
}
