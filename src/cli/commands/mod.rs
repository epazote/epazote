use clap::{
    Arg, ArgAction, ColorChoice, Command,
    builder::{
        FalseyValueParser, ValueParser,
        styling::{AnsiColor, Effects, Styles},
    },
};
use std::{env, fs, path::PathBuf};

pub mod built_info {
    #![allow(clippy::doc_markdown)]
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

/// Remove empty env vars so clap falls back to defaults instead of treating an
/// empty assignment as an explicit value.
pub(crate) fn normalize_env_vars() {
    for name in [
        "EPAZOTE_CONFIG",
        "EPAZOTE_PORT",
        "EPAZOTE_VERBOSE",
        "EPAZOTE_JSON_LOGS",
    ] {
        if matches!(env::var_os(name), Some(value) if value.is_empty()) {
            unsafe {
                env::remove_var(name);
            }
        }
    }
}

pub fn validator_is_file() -> ValueParser {
    ValueParser::from(move |s: &str| -> std::result::Result<PathBuf, String> {
        if let Ok(metadata) = fs::metadata(s)
            && metadata.is_file()
        {
            return Ok(PathBuf::from(s));
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

    let git_hash = built_info::GIT_COMMIT_HASH.unwrap_or("unknown");
    let long_version: &'static str =
        Box::leak(format!("{} - {}", env!("CARGO_PKG_VERSION"), git_hash).into_boxed_str());

    Command::new("epazote")
        .about("Automated HTTP (microservices) supervisor 🌿")
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(long_version)
        .color(ColorChoice::Auto)
        .styles(styles)
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .env("EPAZOTE_CONFIG")
                .help("Path to the configuration file")
                .default_value("epazote.yml")
                .value_parser(validator_is_file())
                .value_name("FILE"),
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .env("EPAZOTE_PORT")
                .help("Port to listen for HTTP metrics")
                .default_value("9080")
                .value_parser(clap::value_parser!(u16))
                .value_name("PORT"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .env("EPAZOTE_VERBOSE")
                .help("Increase verbosity, -vv for debug")
                .action(ArgAction::Count),
        )
        .arg(
            Arg::new("json-logs")
                .long("json-logs")
                .env("EPAZOTE_JSON_LOGS")
                .help("Emit logs in JSON format")
                .action(ArgAction::SetTrue)
                .value_parser(FalseyValueParser::new()),
        )
}

#[cfg(test)]
#[allow(deprecated, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::{ffi::OsStr, ffi::OsString, fs::File, io::Write, sync::Mutex};
    use tempfile::Builder;

    const CONF: &str = r"---
services:
  test:
    url: https://epazote.io
    every: 1m
    expect:
      status: 200
";

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: Option<&OsStr>) -> Self {
            let previous = env::var_os(name);
            unsafe {
                if let Some(value) = value {
                    env::set_var(name, value);
                } else {
                    env::remove_var(name);
                }
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.previous {
                    env::set_var(self.name, value);
                } else {
                    env::remove_var(self.name);
                }
            }
        }
    }

    fn get_config_dir(config: &str) -> tempfile::TempDir {
        let dir = Builder::new()
            .prefix("epazote")
            .tempdir()
            .expect("Failed to create temp dir");
        let file = dir.path().join(config);
        let mut f = File::create(file).expect("Failed to create config file");
        f.write_all(CONF.as_bytes())
            .expect("Failed to write to config file");
        f.flush().expect("Failed to flush config file");
        dir
    }

    fn lock_and_clear_cli_env() -> (std::sync::MutexGuard<'static, ()>, [EnvVarGuard; 4]) {
        let lock = ENV_LOCK.lock().expect("Failed to lock env");
        let guards = [
            EnvVarGuard::set("EPAZOTE_CONFIG", None),
            EnvVarGuard::set("EPAZOTE_PORT", None),
            EnvVarGuard::set("EPAZOTE_VERBOSE", None),
            EnvVarGuard::set("EPAZOTE_JSON_LOGS", None),
        ];
        (lock, guards)
    }

    #[test]
    fn test_help() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("Failed to find bin");
        let assert = cmd.arg("--help").assert();

        assert.stdout(predicate::str::contains(
            "Automated HTTP (microservices) supervisor 🌿",
        ));
    }

    #[test]
    fn test_default_no_config() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("Failed to find bin");
        let assert = cmd.arg("-c no-config.yml").assert();

        assert.stderr(predicate::str::contains(
            "Invalid file path of file does not exists",
        ));
    }

    #[test]
    fn test_default_no_config_in_path() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("Failed to find bin");

        let temp_dir = std::env::temp_dir();

        let assert = cmd.current_dir(temp_dir).assert();

        assert.stderr(predicate::str::contains(
            "Invalid file path of file does not exists",
        ));
    }

    #[test]
    fn test_defaults() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let matches = new().try_get_matches_from(["epazote"]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");

        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some("epazote.yml")
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));
        assert!(!m.get_flag("json-logs"));
    }

    #[test]
    fn test_defaults_no_epazote() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let matches = new().try_get_matches_from(["epazote", "-c", "no-epazote.yml"]);

        assert!(matches.is_err());
    }

    #[test]
    fn test_custom() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let dir = get_config_dir("custom.yml"); // Create temp directory with config file

        let config_file = dir.path().join("custom.yml");

        let matches = new().try_get_matches_from([
            "epazote",
            "-c",
            config_file.to_str().expect("Invalid path"),
            "-p",
            "8080",
        ]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");

        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some(config_file.to_str().expect("Invalid path"))
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(8080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));
        assert!(!m.get_flag("json-logs"));
    }

    #[test]
    fn test_verbose() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let matches = new().try_get_matches_from(["epazote", "-vv", "--json-logs"]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");

        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some("epazote.yml")
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(2));
        assert!(m.get_flag("json-logs"));
    }

    #[test]
    fn test_cli_rejects_empty_config_value() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let matches = new().try_get_matches_from(["epazote", "--config="]);

        assert!(matches.is_err());
    }

    #[test]
    fn test_cli_rejects_empty_port_value() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let matches = new().try_get_matches_from(["epazote", "--port="]);

        assert!(matches.is_err());
    }

    #[test]
    fn test_env_empty_values_fall_back_to_defaults() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let _config = EnvVarGuard::set("EPAZOTE_CONFIG", Some(OsStr::new("")));
        let _port = EnvVarGuard::set("EPAZOTE_PORT", Some(OsStr::new("")));
        let _verbose = EnvVarGuard::set("EPAZOTE_VERBOSE", Some(OsStr::new("")));
        let _json_logs = EnvVarGuard::set("EPAZOTE_JSON_LOGS", Some(OsStr::new("")));

        normalize_env_vars();

        let matches = new().try_get_matches_from(["epazote"]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");
        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some("epazote.yml")
        );
        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));
        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));
        assert!(!m.get_flag("json-logs"));
    }

    #[test]
    fn test_env_values_are_parsed() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let dir = get_config_dir("env-config.yml");
        let config_file = dir.path().join("env-config.yml");

        let _config = EnvVarGuard::set("EPAZOTE_CONFIG", Some(config_file.as_os_str()));
        let _port = EnvVarGuard::set("EPAZOTE_PORT", Some(OsStr::new("9191")));
        let _verbose = EnvVarGuard::set("EPAZOTE_VERBOSE", Some(OsStr::new("2")));
        let _json_logs = EnvVarGuard::set("EPAZOTE_JSON_LOGS", Some(OsStr::new("1")));

        normalize_env_vars();

        let matches = new().try_get_matches_from(["epazote"]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");
        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some(config_file.to_str().expect("Invalid path"))
        );
        assert_eq!(m.get_one::<u16>("port").copied(), Some(9191));
        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(2));
        assert!(m.get_flag("json-logs"));
    }
}
