use super::config::LoggerConfig;
use std::io::Write;

pub fn init(bot_name: &str, config: &LoggerConfig) {
    let bot_name = bot_name.to_string();
    let show_timestamp = config.show_timestamp;
    let show_module = config.show_module;

    env_logger::Builder::new()
        .parse_filters(&config.level)
        .format(move |buf, record| {
            let level = record.level();

            if show_timestamp {
                write!(buf, "{} ", buf.timestamp())?;
            }

            write!(buf, "{}", bot_name)?;
            if show_module {
                let module = record.module_path().unwrap_or("unknown");
                write!(buf, " {}", module)?;
            }

            writeln!(buf, " {:5} {}", level, record.args())
        })
        .init();
}
