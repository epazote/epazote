use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::{fs::File, path::PathBuf, time::Duration};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub config: Option<SmtpConfig>,
    pub services: HashMap<String, ServiceDetails>,
}

#[derive(Debug, Deserialize)]
pub struct SmtpConfig {
    pub smtp: SmtpDetails,
}

#[derive(Debug, Deserialize)]
pub struct SmtpDetails {
    pub username: String,
    pub password: String,
    pub server: String,
    pub port: u16,
    pub headers: SmtpHeaders,
}

#[derive(Debug, Deserialize)]
pub struct SmtpHeaders {
    pub from: String,
    pub to: String,
    pub subject: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceDetails {
    pub url: String,
    #[serde(deserialize_with = "parse_duration")]
    pub every: Duration, // Store as `Duration` for easier usage
    pub expect: Expect,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Expect {
    // Struct name changed to `Expect`
    pub status: u16,
    #[serde(rename = "if_not")]
    pub if_not: Option<IfNot>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IfNot {
    pub cmd: String,
    pub notify: Option<bool>,
}

impl Config {
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let file = File::open(config_path)?;

        let config: Self = serde_yaml::from_reader(file).context("Failed to parse config file")?;

        Ok(config)
    }

    pub fn get_service(&self, service_name: &str) -> Option<&ServiceDetails> {
        self.services.get(service_name)
    }
}

/// Parses a duration string (e.g., "5s", "3m", "1h", "2d") into a `Duration`.
fn parse_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_duration_str(&s).map_err(serde::de::Error::custom)
}

/// Converts a string like "5s", "3m", "1h", "2d" into `Duration`.
fn parse_duration_str(input: &str) -> Result<Duration> {
    let (value, unit) = input.split_at(input.len() - 1);
    let value: u64 = value
        .parse()
        .map_err(|_| anyhow!("Invalid number in duration: {}", input))?;

    match unit {
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        "h" => Ok(Duration::from_secs(value * 60 * 60)),
        "d" => Ok(Duration::from_secs(value * 60 * 60 * 24)),
        _ => Err(anyhow!("Invalid duration unit: {}", unit)),
    }
}
