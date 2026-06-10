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
        "EPAZOTE_BIND",
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

/// Parses `EPAZOTE_VERBOSE` values with a helpful error: the variable takes a
/// number equivalent to repeating `-v` (1=info, 2=debug, 3+=trace).
fn verbosity_parser() -> ValueParser {
    ValueParser::from(|s: &str| -> std::result::Result<u8, String> {
        s.parse::<u8>().map_err(|_| {
            format!("expected a number, e.g. EPAZOTE_VERBOSE=2 equals -vv (got '{s}')")
        })
    })
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
            Arg::new("bind")
                .short('b')
                .long("bind")
                .env("EPAZOTE_BIND")
                .help("Address to bind the metrics server (e.g. 127.0.0.1 or ::1 to keep it local)")
                .default_value("[::]")
                .value_name("ADDRESS"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .env("EPAZOTE_VERBOSE")
                .help("Increase verbosity, -vv for debug (EPAZOTE_VERBOSE=2)")
                .action(ArgAction::Count)
                .value_parser(verbosity_parser()),
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
    use crate::cli::test_env::{EnvVarGuard, lock_and_clear_cli_env};
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::{ffi::OsStr, fs::File, io::Write};
    use tempfile::Builder;

    const CONF: &str = r"---
services:
  test:
    url: https://epazote.io
    every: 1m
    expect:
      status: 200
";

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
    fn test_invalid_body_regex_halts_startup() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let dir = Builder::new()
            .prefix("epazote")
            .tempdir()
            .expect("Failed to create temp dir");
        let file = dir.path().join("bad-regex.yml");
        let mut f = File::create(&file).expect("Failed to create config file");
        f.write_all(
            br#"---
services:
  test:
    url: https://epazote.io
    every: 1m
    expect:
      status: 200
      body: r"(unclosed"
"#,
        )
        .expect("Failed to write config");
        f.flush().expect("Failed to flush config");

        let mut cmd = Command::cargo_bin(env!("CARGO_PKG_NAME")).expect("Failed to find bin");
        let assert = cmd.arg("-c").arg(&file).assert();

        assert
            .failure()
            .stderr(predicate::str::contains("invalid regex in 'expect.body'"));
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

        assert_eq!(
            m.get_one::<String>("bind").map(String::as_str),
            Some("[::]")
        );

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
            "-b",
            "127.0.0.1",
        ]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");

        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some(config_file.to_str().expect("Invalid path"))
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(8080));

        assert_eq!(
            m.get_one::<String>("bind").map(String::as_str),
            Some("127.0.0.1")
        );

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
    fn test_env_verbose_numeric_values_map_to_count() {
        // Regression for issue #19: EPAZOTE_VERBOSE=N must behave like -v
        // repeated N times.
        let (_lock, _env) = lock_and_clear_cli_env();

        for (value, expected) in [("1", 1u8), ("2", 2), ("3", 3)] {
            let _verbose = EnvVarGuard::set("EPAZOTE_VERBOSE", Some(OsStr::new(value)));
            let matches = new().try_get_matches_from(["epazote"]);

            let m = matches.expect("Matches should be present");
            assert_eq!(
                m.get_one::<u8>("verbose").copied(),
                Some(expected),
                "EPAZOTE_VERBOSE={value} should map to count {expected}"
            );
        }
    }

    #[test]
    fn test_env_verbose_invalid_value_reports_helpful_error() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let _verbose = EnvVarGuard::set("EPAZOTE_VERBOSE", Some(OsStr::new("vvv")));

        let matches = new().try_get_matches_from(["epazote"]);

        let err = matches.expect_err("EPAZOTE_VERBOSE=vvv should be rejected");
        assert!(
            err.to_string()
                .contains("expected a number, e.g. EPAZOTE_VERBOSE=2 equals -vv"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_cli_flags_take_precedence_over_env_verbose() {
        let (_lock, _env) = lock_and_clear_cli_env();
        let _verbose = EnvVarGuard::set("EPAZOTE_VERBOSE", Some(OsStr::new("3")));

        let matches = new().try_get_matches_from(["epazote", "-v"]);

        let m = matches.expect("Matches should be present");
        // CLI occurrences win over the env var, so a stray -v on the command
        // line caps verbosity regardless of EPAZOTE_VERBOSE.
        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(1));
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
        let _bind = EnvVarGuard::set("EPAZOTE_BIND", Some(OsStr::new("")));
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
        assert_eq!(
            m.get_one::<String>("bind").map(String::as_str),
            Some("[::]")
        );
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
        let _bind = EnvVarGuard::set("EPAZOTE_BIND", Some(OsStr::new("::1")));
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
        assert_eq!(m.get_one::<String>("bind").map(String::as_str), Some("::1"));
        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(2));
        assert!(m.get_flag("json-logs"));
    }
}
