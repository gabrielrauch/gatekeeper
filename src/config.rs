use ipnet::IpNet;
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
    #[serde(default)]
    pub access: AccessConfig,
    #[serde(default)]
    pub policy: Vec<PolicyConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AccessConfig {
    #[serde(default)]
    pub allowlist_ips: Vec<String>,
    #[serde(default)]
    pub allowlist_keys: Vec<String>,
    #[serde(default)]
    pub denylist_ips: Vec<String>,
    #[serde(default)]
    pub denylist_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicyConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    pub algorithm: Option<String>,
    pub capacity: Option<u64>,
    pub refill_rate: Option<f64>,
    pub cost: Option<u64>,
    pub identify_by: Option<String>,
    pub bypass: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatchRule {
    pub path: String,
    pub methods: Option<Vec<String>>,
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

        // Validate policies
        let mut seen_names = std::collections::HashSet::new();
        for policy in &self.policy {
            if policy.name.is_empty() {
                return Err(ConfigError::Validation(
                    "policy name must not be empty".to_string(),
                ));
            }
            if !seen_names.insert(policy.name.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate policy name '{}'",
                    policy.name
                )));
            }
            if let Some(ref algorithm) = policy.algorithm {
                if algorithm != "token_bucket" {
                    return Err(ConfigError::Validation(format!(
                        "policy '{}': unsupported algorithm '{}'; must be 'token_bucket'",
                        policy.name, algorithm
                    )));
                }
            }
            if let Some(ref identify_by) = policy.identify_by {
                if identify_by != "ip" {
                    return Err(ConfigError::Validation(format!(
                        "policy '{}': unsupported identify_by '{}'; must be 'ip'",
                        policy.name, identify_by
                    )));
                }
            }
            if let Some(capacity) = policy.capacity {
                if capacity == 0 {
                    return Err(ConfigError::Validation(format!(
                        "policy '{}': capacity must be greater than 0",
                        policy.name
                    )));
                }
            }
            if let Some(refill_rate) = policy.refill_rate {
                if refill_rate <= 0.0 {
                    return Err(ConfigError::Validation(format!(
                        "policy '{}': refill_rate must be greater than 0",
                        policy.name
                    )));
                }
            }
        }

        // Validate access IP lists
        for ip in self.access.allowlist_ips.iter().chain(self.access.denylist_ips.iter()) {
            ip.parse::<IpNet>().map_err(|e| {
                ConfigError::Validation(format!("invalid IP/CIDR '{}': {e}", ip))
            })?;
        }

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

    #[test]
    fn parses_config_with_policies() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]

[access]
allowlist_ips = ["10.0.0.0/8", "192.168.1.0/24"]
denylist_ips = ["203.0.113.0/24"]

[[policy]]
name = "api-strict"
algorithm = "token_bucket"
capacity = 50
refill_rate = 5.0
cost = 2
identify_by = "ip"
bypass = false

[policy.match]
path = "/api/*"
methods = ["GET", "POST"]

[[policy]]
name = "health-bypass"
bypass = true

[policy.match]
path = "/health"
"#;
        let cfg = parse(toml).expect("config with policies should parse");
        assert_eq!(cfg.policy.len(), 2);
        assert_eq!(cfg.policy[0].name, "api-strict");
        assert_eq!(cfg.policy[0].match_rule.path, "/api/*");
        assert_eq!(cfg.policy[0].match_rule.methods, Some(vec!["GET".to_string(), "POST".to_string()]));
        assert_eq!(cfg.policy[0].capacity, Some(50));
        assert_eq!(cfg.policy[0].refill_rate, Some(5.0));
        assert_eq!(cfg.policy[0].cost, Some(2));
        assert_eq!(cfg.policy[0].bypass, Some(false));
        assert_eq!(cfg.policy[1].name, "health-bypass");
        assert_eq!(cfg.policy[1].bypass, Some(true));
        assert!(cfg.policy[1].match_rule.methods.is_none());
        assert_eq!(cfg.access.allowlist_ips, vec!["10.0.0.0/8", "192.168.1.0/24"]);
        assert_eq!(cfg.access.denylist_ips, vec!["203.0.113.0/24"]);
    }

    #[test]
    fn validates_duplicate_policy_names() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]

[[policy]]
name = "dup"

[policy.match]
path = "/a"

[[policy]]
name = "dup"

[policy.match]
path = "/b"
"#;
        let err = parse(toml).expect_err("duplicate policy names should fail");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn validates_unsupported_algorithm() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]

[[policy]]
name = "bad-algo"
algorithm = "sliding_window"

[policy.match]
path = "/api/*"
"#;
        let err = parse(toml).expect_err("unsupported algorithm should fail");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("algorithm"));
    }

    #[test]
    fn validates_invalid_cidr() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]

[access]
denylist_ips = ["not-an-ip"]
"#;
        let err = parse(toml).expect_err("invalid CIDR should fail");
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("not-an-ip"));
    }

    #[test]
    fn empty_policies_is_valid() {
        let toml = r#"
[server]
listen = "127.0.0.1:8080"
upstream_url = "http://localhost:3000"

[defaults]
capacity = 100
refill_rate = 10.0

[store.memory]
"#;
        let cfg = parse(toml).expect("no policies should be valid");
        assert!(cfg.policy.is_empty());
        assert!(cfg.access.allowlist_ips.is_empty());
        assert!(cfg.access.denylist_ips.is_empty());
    }
}
