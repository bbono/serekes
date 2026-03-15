use env_logger::fmt::style::{AnsiColor, Style};
use log::Level;
use std::io::Write;

pub fn init(bot_name: &str, level: &str) {
    let bot_name = bot_name.to_string();

    env_logger::Builder::new()
        .parse_filters(level)
        .format(move |buf, record| {
            let level = record.level();
            let module = record.module_path().unwrap_or("unknown");
            let style = level_style(level);

            writeln!(
                buf,
                "{} {} {} {style}{:5}{style:#} {}",
                buf.timestamp(),
                bot_name,
                module,
                level,
                record.args()
            )
        })
        .init();
}

fn level_style(level: Level) -> Style {
    let color = match level {
        Level::Error => AnsiColor::Red,
        Level::Warn => AnsiColor::Yellow,
        Level::Info => AnsiColor::Green,
        Level::Debug => AnsiColor::Blue,
        Level::Trace => AnsiColor::Cyan,
    };
    Style::new().fg_color(Some(color.into()))
}
