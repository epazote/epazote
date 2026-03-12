use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize};
use std::str::FromStr;
use std::{collections::HashMap, fs::File, path::PathBuf, time::Duration};
use strum::{Display, EnumString};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub services: HashMap<String, ServiceDetails>,
}

impl Config {
    /// Creates a new `Config` from a YAML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, parsed, or contains invalid service configurations.
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let file = File::open(config_path)?;

        let config: Self = serde_yaml::from_reader(file).context("Failed to parse config file")?;

        // Validate all services after loading
        for (name, service) in &config.services {
            service
                .validate()
                .with_context(|| format!("Invalid configuration for service '{name}'"))?;
        }

        Ok(config)
    }

    #[must_use]
    pub fn get_service(&self, service_name: &str) -> Option<&ServiceDetails> {
        self.services.get(service_name)
    }
}

#[derive(Default, Debug, Clone, Copy, EnumString, Display, Serialize, PartialEq, Eq)]
#[strum(serialize_all = "UPPERCASE")] // Ensures correct casing for HTTP methods
pub enum HttpMethod {
    Connect,
    Delete,

    #[default]
    Get,

    Head,
    Options,
    Patch,
    Post,
    Put,
    Trace,
}

// Custom deserialization for case-insensitive HTTP methods
impl<'de> Deserialize<'de> for HttpMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let method = String::deserialize(deserializer)?;
        Self::from_str(&method.to_uppercase()).map_err(serde::de::Error::custom)
    }
}

const fn default_http_method() -> HttpMethod {
    HttpMethod::Get
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", untagged)]
pub enum BodyType {
    Json(serde_json::Value),       // Covers structured JSON data
    Form(HashMap<String, String>), // Covers form-encoded data
    Text(String),                  // Covers plain text, XML, and other string-based data
}

impl<'de> Deserialize<'de> for BodyType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;

        if let Some(json_value) = value.get("json") {
            return Ok(Self::Json(json_value.clone()));
        }

        if let Some(form) = value.get("form") {
            let form_map = serde_json::from_value::<HashMap<String, String>>(form.clone())
                .map_err(serde::de::Error::custom)?;
            return Ok(Self::Form(form_map));
        }

        if let Some(text) = value.as_str() {
            return Ok(Self::Text(text.to_string()));
        }

        serde_json::from_value(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServiceDetails {
    #[serde(deserialize_with = "parse_duration")]
    pub every: Duration,

    pub expect: Expect,

    pub follow_redirects: Option<bool>,

    pub headers: Option<HashMap<String, String>>,

    #[serde(rename = "max_bytes")]
    pub max_bytes: Option<usize>,

    pub test: Option<String>,

    #[serde(deserialize_with = "parse_duration", default = "default_timeout")]
    pub timeout: Duration,

    pub url: Option<String>,

    #[serde(default = "default_http_method")]
    pub method: HttpMethod,

    #[serde(default)]
    pub body: Option<BodyType>,
}

impl ServiceDetails {
    /// Validates the service configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid (e.g., missing both URL and test command).
    pub fn validate(&self) -> Result<()> {
        match (&self.url, &self.test) {
            (Some(_), Some(_)) => Err(anyhow!("Service cannot have both 'url' and 'test'.")),
            (None, None) => Err(anyhow!("Service must have either 'url' or 'test'.")),
            _ => self.expect.validate(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Expect {
    pub status: u16, // Use for both HTTP & text exit codes
    pub header: Option<HashMap<String, String>>,
    pub body: Option<String>,
    pub json: Option<serde_json::Value>,

    #[serde(rename = "if_not")]
    pub if_not: Option<Action>,
}

impl Expect {
    /// Validates that the response expectation is internally consistent.
    ///
    /// # Errors
    ///
    /// Returns an error if incompatible expectation types are configured together.
    pub fn validate(&self) -> Result<()> {
        if self.body.is_some() && self.json.is_some() {
            return Err(anyhow!(
                "Expect cannot have both 'body' and 'json' configured."
            ));
        }

        Ok(())
    }
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct Action {
    pub cmd: Option<String>,
    pub http: Option<String>,
    pub stop: Option<usize>,
    pub threshold: Option<usize>,
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
        .map_err(|_| anyhow!("Invalid number in duration: {input}"))?;

    match unit {
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        "h" => Ok(Duration::from_secs(value * 60 * 60)),
        "d" => Ok(Duration::from_secs(value * 60 * 60 * 24)),
        _ => Err(anyhow!("Invalid duration unit: {unit}")),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    // Helper to create config from YAML
    fn create_config(yaml: &str) -> tempfile::NamedTempFile {
        let mut tmp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        tmp_file
            .write_all(yaml.as_bytes())
            .expect("Failed to write to temp file");
        tmp_file.flush().expect("Failed to flush temp file");
        tmp_file
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(
            parse_duration_str("5s").expect("Failed to parse duration"),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_duration_str("3m").expect("Failed to parse duration"),
            Duration::from_secs(180)
        );
        assert_eq!(
            parse_duration_str("1h").expect("Failed to parse duration"),
            Duration::from_secs(3600)
        );
        assert_eq!(
            parse_duration_str("2d").expect("Failed to parse duration"),
            Duration::from_secs(172_800)
        );
    }

    #[test]
    fn test_config() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
      ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        assert_eq!(config.services.len(), 1);
        assert_eq!(
            config.services.get("test").expect("Service not found").url,
            Some("https://epazote.io".to_string())
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .every,
            Duration::from_secs(30)
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .headers
                .as_ref()
                .expect("Headers not found")
                .get("X-Custom-Header")
                .expect("Header not found"),
            "TestValue"
        );
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .expect
                .status,
            200
        );

        // check method
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .method,
            HttpMethod::Get
        );

        // follow_redirects is not set
        assert_eq!(
            config
                .services
                .get("test")
                .expect("Service not found")
                .follow_redirects,
            None
        );
    }

    #[test]
    fn test_bad_config_url_and_test() {
        let yaml = r#"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
    test: "echo test"
      "#;

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_bad_config_missing_url_and_test() {
        let yaml = r"
---
services:
  test:
    every: 30s
    headers:
      X-Custom-Header: TestValue
    expect:
      status: 200
      ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_all_http_methods_case_insensitive() {
        let methods = [
            "GET", "get", "Get", "POST", "post", "Post", "PUT", "put", "Put", "DELETE", "delete",
            "Delete", "PATCH", "patch", "Patch", "HEAD", "head", "Head", "OPTIONS", "options",
            "Options", "CONNECT", "connect", "Connect", "TRACE", "trace", "Trace",
        ];

        for method in methods {
            let yaml = format!(
                r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    method: {method}
    expect:
      status: 200
"
            );

            let tmp_file = create_config(&yaml);
            let config_file = tmp_file.path().to_path_buf();
            let config = Config::new(config_file).expect("Failed to load config");

            assert_eq!(
                config
                    .services
                    .get("test")
                    .expect("Service not found")
                    .method
                    .to_string(),
                method.to_uppercase(),
                "Failed for method: {method}"
            );
        }
    }

    #[test]
    fn test_body_type_json() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    method: POST
    body:
      json:
        key: value
        oi: hola
    every: 30s
    expect:
      status: 200
    ";

        let expected_json = json!({
            "key": "value",
            "oi": "hola"
        });

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let service = config.services.get("test").expect("Service not found");
        let body = service.body.as_ref().expect("Body not found");

        assert_eq!(body, &BodyType::Json(expected_json));
    }

    #[test]
    fn test_expect_json() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      json:
        status: success
        data:
          activeTargets:
            - health: up
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let expected_json = json!({
            "status": "success",
            "data": {
                "activeTargets": [
                    { "health": "up" }
                ]
            }
        });

        let service = config.services.get("test").expect("Service not found");
        let body = service
            .expect
            .json
            .as_ref()
            .expect("JSON expectation not found");

        assert_eq!(body, &expected_json);
    }

    #[test]
    fn test_expect_body_and_json_are_mutually_exclusive() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      body: success
      json:
        status: success
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file);

        assert!(config.is_err());
    }

    #[test]
    fn test_expect_if_not_threshold_and_stop() {
        let yaml = r"
---
services:
  test:
    url: https://epazote.io
    every: 30s
    expect:
      status: 200
      json:
        status: success
      if_not:
        threshold: 3
        stop: 2
        cmd: systemctl restart test
    ";

        let tmp_file = create_config(yaml);
        let config_file = tmp_file.path().to_path_buf();
        let config = Config::new(config_file).expect("Failed to load config");

        let service = config.services.get("test").expect("Service not found");
        let if_not = service.expect.if_not.as_ref().expect("if_not not found");

        assert_eq!(if_not.threshold, Some(3));
        assert_eq!(if_not.stop, Some(2));
        assert_eq!(if_not.cmd.as_deref(), Some("systemctl restart test"));
    }
}
