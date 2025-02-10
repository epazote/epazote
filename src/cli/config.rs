use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs::File, path::PathBuf, time::Duration};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub services: HashMap<String, ServiceDetails>,
}

impl Config {
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let file = File::open(config_path)?;

        let config: Self = serde_yaml::from_reader(file).context("Failed to parse config file")?;

        // Validate all services after loading
        for (name, service) in &config.services {
            service
                .validate()
                .with_context(|| format!("Invalid configuration for service '{}'", name))?;
        }

        Ok(config)
    }

    pub fn get_service(&self, service_name: &str) -> Option<&ServiceDetails> {
        self.services.get(service_name)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceDetails {
    #[serde(deserialize_with = "parse_duration")]
    pub every: Duration,

    pub expect: Expect,
    pub follow_redirects: Option<bool>,
    pub headers: Option<HashMap<String, String>>,

    #[serde(rename = "if_header")]
    pub if_header: Option<HashMap<String, Action>>,

    #[serde(rename = "if_status")]
    pub if_status: Option<HashMap<String, Action>>,

    pub insecure: Option<bool>,

    #[serde(rename = "read_limit")]
    pub read_limit: Option<i64>,

    pub stop: Option<i8>,
    pub test: Option<String>,

    #[serde(deserialize_with = "parse_duration", default = "default_timeout")]
    pub timeout: Duration,

    pub url: Option<String>,
}

impl ServiceDetails {
    pub fn validate(&self) -> Result<()> {
        match (&self.url, &self.test) {
            (Some(_), Some(_)) => Err(anyhow!("Service cannot have both 'url' and 'test'.")),
            (None, None) => Err(anyhow!("Service must have either 'url' or 'test'.")),
            _ => Ok(()), // Now expect is always required
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Expect {
    pub status: u16, // Use for both HTTP & text exit codes
    pub header: Option<HashMap<String, String>>,
    pub body: Option<String>,

    #[serde(rename = "if_not")]
    pub if_not: Option<Action>,
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct Action {
    pub cmd: String,
}

// Default timeout value
const fn default_timeout() -> Duration {
    Duration::from_secs(5)
}

/// Parses a duration string (e.g., "5s", "3m", "1h", "2d") into a Duration.
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
