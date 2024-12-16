use clap::{
    builder::{
        styling::{AnsiColor, Effects, Styles},
        ValueParser,
    },
    Arg, ArgAction, ColorChoice, Command,
};
use std::{env, fs, path::PathBuf};

pub fn validator_is_file() -> ValueParser {
    ValueParser::from(move |s: &str| -> std::result::Result<PathBuf, String> {
        if let Ok(metadata) = fs::metadata(s) {
            if metadata.is_file() {
                return Ok(PathBuf::from(s));
            }
        }

        Err(format!("Invalid file path of file does not exists: '{s}'"))
    })
}

pub fn new() -> Command {
    let styles = Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default());

    Command::new("epazote")
        .about("Automated HTTP (microservices) supervisor 🌿")
        .version(env!("CARGO_PKG_VERSION"))
        .color(ColorChoice::Auto)
        .styles(styles)
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .help("Path to the configuration file")
                .default_value("epazote.yml")
                .value_parser(validator_is_file())
                .value_name("FILE"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Increase verbosity, -vv for debug")
                .action(ArgAction::Count),
        )
}
