use super::config::LoggerConfig;
use env_logger::fmt::Color;
use std::io::Write;

pub fn init(bot_name: &str, config: &LoggerConfig) {
    let bot_name = bot_name.to_string();
    let show_timestamp = config.show_timestamp;
    let show_module = config.show_module;

    env_logger::Builder::new()
        .parse_filters(&config.level)
        .format(move |buf, record| {
            let level = record.level();
            let mut level_style = buf.style();
            match level {
                log::Level::Error => {
                    level_style.set_color(Color::Red).set_bold(true);
                }
                log::Level::Warn => {
                    level_style.set_color(Color::Yellow).set_bold(true);
                }
                log::Level::Info => {
                    level_style.set_color(Color::Green);
                }
                log::Level::Debug => {
                    level_style.set_color(Color::Cyan);
                }
                log::Level::Trace => {
                    level_style.set_color(Color::Magenta);
                }
            };

            if show_timestamp {
                write!(buf, "{} ", buf.timestamp())?;
            }

            write!(buf, "{}", bot_name)?;

            if show_module {
                let module = record.file().unwrap_or("unknown");
                let module = module
                    .strip_prefix("src/")
                    .unwrap_or(module)
                    .strip_suffix(".rs")
                    .unwrap_or(module)
                    .replace('/', "::");
                write!(buf, "::{}", module)?;
            }

            writeln!(
                buf,
                " {} {}",
                level_style.value(format_args!("{:5}", level)),
                level_style.value(record.args())
            )
        })
        .init();
}
