use crate::cli::actions::Action;
use anyhow::Result;
use std::path::PathBuf;

pub fn handler(matches: &clap::ArgMatches) -> Result<Action> {
    Ok(Action::Run {
        config: matches.get_one::<PathBuf>("config").unwrap().to_path_buf(),
        port: matches.get_one::<u16>("port").copied().unwrap_or(9080),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::commands::new;
    use std::io::Write;

    const CONF: &str = r#"---
services:
  test:
    url: https://epazote.io
    every: 1m
    expect:
      status: 200
"#;

    // Helper to create config from YAML
    fn create_config() -> Result<tempfile::NamedTempFile> {
        let mut tmp_file = tempfile::NamedTempFile::new().unwrap();
        tmp_file.write_all(CONF.as_bytes()).unwrap();
        tmp_file.flush().unwrap();
        Ok(tmp_file)
    }

    #[test]
    fn test_handler() -> Result<()> {
        let tmp_config = create_config()?;

        let config_path = tmp_config.path().to_path_buf();

        let matches =
            new().try_get_matches_from(["epazote", "--config", config_path.to_str().unwrap()]);

        assert!(matches.is_ok());

        let m = matches.unwrap();

        assert_eq!(
            m.get_one::<PathBuf>("config").map(|p| p.to_str().unwrap()),
            Some(config_path.to_str().unwrap())
        );

        assert_eq!(m.get_one::<u16>("port").copied(), Some(9080));

        assert_eq!(m.get_one::<u8>("verbose").copied(), Some(0));

        let action = handler(&m)?;

        match action {
            Action::Run { config, port } => {
                assert_eq!(config, config_path);
                assert_eq!(port, 9080);
            }
        }

        Ok(())
    }
}
