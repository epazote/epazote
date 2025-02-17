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
            Arg::new("port")
                .short('p')
                .long("port")
                .help("Port to listen for HTTP metrics")
                .default_value("9080")
                .value_parser(clap::value_parser!(u16))
                .value_name("PORT"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .help("Increase verbosity, -vv for debug")
                .action(ArgAction::Count),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::Builder;

    const CONF: &str = r#"---
services:
  test:
    url: https://epazote.io
    every: 1m
    expect:
      status: 200
"#;

    fn get_config_dir(config: &str) -> Result<tempfile::TempDir> {
        let dir = Builder::new().prefix("epazote").tempdir().unwrap();
        let file = dir.path().join(config);
        let mut f = File::create(&file).unwrap();
        f.write_all(CONF.as_bytes()).unwrap();
        f.flush().unwrap();
        Ok(dir)
    }

    #[test]
    fn test_help() {
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
        let assert = cmd.arg("--help").assert();

        assert.stdout(predicate::str::contains(
            "Automated HTTP (microservices) supervisor 🌿",
        ));
    }

    #[test]
    fn test_default_no_config() {
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
        let assert = cmd.arg("-c no-config.yml").assert();

        assert.stderr(predicate::str::contains(
            "Invalid file path of file does not exists",
        ));
    }

    #[test]
    fn test_default_no_config_in_path() -> Result<()> {
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).unwrap();
        let assert = cmd.current_dir("/tmp").assert();

        assert.stderr(predicate::str::contains(
            "Invalid file path of file does not exists",
        ));

        Ok(())
    }

    #[test]
    fn test_defaults() -> Result<()> {
        let matches = new().try_get_matches_from(["epazote"]);

        assert!(matches.is_ok());

        let m = matches.unwrap();

        assert_eq!(
            m.get_one::<PathBuf>("config").map(|p| p.to_str().unwrap()),
            Some("epazote.yml")
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));

        Ok(())
    }

    #[test]
    fn test_defaults_no_epazote() -> Result<()> {
        let matches = new().try_get_matches_from(["epazote", "-c", "no-epazote.yml"]);

        assert!(matches.is_err());

        Ok(())
    }

    #[test]
    fn test_custom() -> Result<()> {
        let dir = get_config_dir("custom.yml")?; // Create temp directory with config file

        let config_file = dir.path().join("custom.yml");

        let matches = new().try_get_matches_from([
            "epazote",
            "-c",
            config_file.to_str().unwrap(),
            "-p",
            "8080",
        ]);

        assert!(matches.is_ok());

        let m = matches.unwrap();

        assert_eq!(
            m.get_one::<PathBuf>("config").map(|p| p.to_str().unwrap()),
            Some(config_file.to_str().unwrap())
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(8080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));

        Ok(())
    }

    #[test]
    fn test_verbose() -> Result<()> {
        let matches = new().try_get_matches_from(["epazote", "-vv"]);

        assert!(matches.is_ok());

        let m = matches.unwrap();

        assert_eq!(
            m.get_one::<PathBuf>("config").map(|p| p.to_str().unwrap()),
            Some("epazote.yml")
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(2));

        Ok(())
    }
}
