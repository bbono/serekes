use std::io::Write;

pub fn init(bot_name: &str, level: &str) {
    let bot_name = bot_name.to_string();

    env_logger::Builder::new()
        .parse_filters(level)
        .format(move |buf, record| {
            let level = record.level();
            let module = record.module_path().unwrap_or("unknown");

            writeln!(buf, "{} {} {} {:5} {}", buf.timestamp(), bot_name, module, level, record.args())
        })
        .init();
}
