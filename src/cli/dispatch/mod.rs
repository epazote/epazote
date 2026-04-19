use crate::cli::actions::Action;
use std::path::PathBuf;

#[allow(clippy::unnecessary_wraps, clippy::expect_used)]
pub fn handler(matches: &clap::ArgMatches) -> Action {
    Action::Run {
        config: matches
            .get_one::<PathBuf>("config")
            .expect("Config path must be present due to default value")
            .clone(),
        port: matches
            .get_one::<u16>("port")
            .copied()
            .expect("Port must be present due to default value"),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::commands::{new, normalize_env_vars};
    use std::{ffi::OsString, io::Write, sync::Mutex};

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
        fn clear(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            unsafe {
                std::env::remove_var(name);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.previous {
                    std::env::set_var(self.name, value);
                } else {
                    std::env::remove_var(self.name);
                }
            }
        }
    }

    // Helper to create config from YAML
    fn create_config() -> tempfile::NamedTempFile {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(CONF.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        tmp_file
    }

    #[test]
    fn test_handler() {
        let _lock = ENV_LOCK.lock().expect("Failed to lock env");
        let _config_env = EnvVarGuard::clear("EPAZOTE_CONFIG");
        let _port_env = EnvVarGuard::clear("EPAZOTE_PORT");
        let _verbose_env = EnvVarGuard::clear("EPAZOTE_VERBOSE");
        let _json_logs_env = EnvVarGuard::clear("EPAZOTE_JSON_LOGS");
        normalize_env_vars();

        let tmp_config = create_config();

        let config_path = tmp_config.path().to_path_buf();

        let matches = new().try_get_matches_from([
            "epazote",
            "--config",
            config_path.to_str().expect("Invalid path"),
        ]);

        assert!(matches.is_ok());

        let m = matches.expect("Matches should be present");

        assert_eq!(
            m.get_one::<PathBuf>("config")
                .map(|p| p.to_str().expect("Invalid path")),
            Some(config_path.to_str().expect("Invalid path"))
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));

        let action = handler(&m);

        match action {
            Action::Run { config, port } => {
                assert_eq!(config, config_path);
                assert_eq!(port, 9080);
            }
        }
    }
}
