use serde::{Deserialize, Deserializer};
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {source}")]
    ReadFile {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Duration deserializer
// ---------------------------------------------------------------------------

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub defaults: DefaultsConfig,
    pub store: StoreConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub upstream_url: String,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    #[serde(deserialize_with = "deserialize_duration", default = "default_connect_timeout")]
    pub connect_timeout: Duration,
    #[serde(deserialize_with = "deserialize_duration", default = "default_request_timeout")]
    pub request_timeout: Duration,
}

fn default_connect_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(30)
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            connect_timeout: default_connect_timeout(),
            request_timeout: default_request_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
    Open,
    Closed,
}

impl Default for FailMode {
    fn default() -> Self {
        FailMode::Open
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    pub capacity: u64,
    pub refill_rate: f64,
    #[serde(default = "default_cost")]
    pub cost: u64,
    #[serde(default)]
    pub fail_mode: FailMode,
}

fn default_algorithm() -> String {
    "token_bucket".to_string()
}

fn default_cost() -> u64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    pub memory: MemoryStoreConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryStoreConfig {
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
    #[serde(deserialize_with = "deserialize_duration", default = "default_eviction_interval")]
    pub eviction_interval: Duration,
}

fn default_max_entries() -> usize {
    1_000_000
}

fn default_eviction_interval() -> Duration {
    Duration::from_secs(60)
}

impl Default for MemoryStoreConfig {
    fn default() -> Self {
        Self {
            max_entries: default_max_entries(),
            eviction_interval: default_eviction_interval(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config::from_file
// ---------------------------------------------------------------------------

impl Config {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let contents =
            std::fs::read_to_string(path).map_err(|source| ConfigError::ReadFile {
                path: path.to_string(),
                source,
            })?;

        let config: Config = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.defaults.capacity == 0 {
            return Err(ConfigError::Validation(
                "defaults.capacity must be greater than 0".to_string(),
            ));
        }
        if self.defaults.refill_rate <= 0.0 {
            return Err(ConfigError::Validation(
                "defaults.refill_rate must be greater than 0".to_string(),
            ));
        }
        if self.defaults.cost == 0 {
            return Err(ConfigError::Validation(
                "defaults.cost must be greater than 0".to_string(),
            ));
        }
        self.server
            .upstream_url
            .parse::<http::Uri>()
            .map_err(|e| {
                ConfigError::Validation(format!("invalid upstream_url '{}': {e}", self.server.upstream_url))
            })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(toml)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_minimal_config() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]
"#;
        let cfg = parse(toml).expect("minimal config should parse");

        // server defaults
        assert_eq!(cfg.server.proxy.connect_timeout, Duration::from_secs(5));
        assert_eq!(cfg.server.proxy.request_timeout, Duration::from_secs(30));

        // defaults section defaults
        assert_eq!(cfg.defaults.algorithm, "token_bucket");
        assert_eq!(cfg.defaults.cost, 1);
        assert_eq!(cfg.defaults.fail_mode, FailMode::Open);

        // store defaults
        assert_eq!(cfg.store.memory.max_entries, 1_000_000);
        assert_eq!(cfg.store.memory.eviction_interval, Duration::from_secs(60));
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
[server]
listen = "0.0.0.0:9090"
upstream_url = "http://backend:8000"

[server.proxy]
connect_timeout = "10s"
request_timeout = "60s"

[defaults]
algorithm = "sliding_window"
capacity = 500
refill_rate = 50.0
cost = 2
fail_mode = "closed"

[store.memory]
max_entries = 500_000
eviction_interval = "120s"
"#;
        let cfg = parse(toml).expect("full config should parse");

        assert_eq!(cfg.server.listen.to_string(), "0.0.0.0:9090");
        assert_eq!(cfg.server.upstream_url, "http://backend:8000");
        assert_eq!(cfg.server.proxy.connect_timeout, Duration::from_secs(10));
        assert_eq!(cfg.server.proxy.request_timeout, Duration::from_secs(60));

        assert_eq!(cfg.defaults.algorithm, "sliding_window");
        assert_eq!(cfg.defaults.capacity, 500);
        assert_eq!(cfg.defaults.refill_rate, 50.0);
        assert_eq!(cfg.defaults.cost, 2);
        assert_eq!(cfg.defaults.fail_mode, FailMode::Closed);

        assert_eq!(cfg.store.memory.max_entries, 500_000);
        assert_eq!(cfg.store.memory.eviction_interval, Duration::from_secs(120));
    }

    #[test]
    fn validates_zero_capacity() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 0
refill_rate = 10.0

[store.memory]
"#;
        let err = parse(toml).expect_err("zero capacity should fail validation");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("capacity"));
    }

    #[test]
    fn validates_negative_refill_rate() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = -1.0

[store.memory]
"#;
        let err = parse(toml).expect_err("negative refill_rate should fail validation");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("refill_rate"));
    }

    #[test]
    fn validates_invalid_upstream_url() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "not a url %%"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]
"#;
        let err = parse(toml).expect_err("invalid upstream_url should fail validation");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("upstream_url"));
    }
}
