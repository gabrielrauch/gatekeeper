pub mod matcher;

use std::sync::Arc;

use crate::algorithm::token_bucket::TokenBucket;
use crate::algorithm::RateLimiter;
use crate::config::{Config, MatchRule};
use crate::identity::ip::IpExtractor;
use crate::identity::IdentityExtractor;
use crate::store::memory::MemoryStore;

use matcher::policy_matches;

// ---------------------------------------------------------------------------
// ResolvedPolicy
// ---------------------------------------------------------------------------

pub struct ResolvedPolicy {
    pub name: String,
    pub limiter: Arc<dyn RateLimiter>,
    pub extractor: Arc<dyn IdentityExtractor>,
    pub cost: u64,
    pub bypass: bool,
}

// ---------------------------------------------------------------------------
// PolicyEngine
// ---------------------------------------------------------------------------

pub struct PolicyEngine {
    policies: Vec<(MatchRule, ResolvedPolicy)>,
    default_policy: ResolvedPolicy,
}

impl PolicyEngine {
    pub fn new(config: &Config, store: Arc<MemoryStore>) -> Self {
        let defaults = &config.defaults;

        let mut policies = Vec::new();
        for policy_cfg in &config.policy {
            let capacity = policy_cfg.capacity.unwrap_or(defaults.capacity);
            let refill_rate = policy_cfg.refill_rate.unwrap_or(defaults.refill_rate);
            let cost = policy_cfg.cost.unwrap_or(defaults.cost);
            let bypass = policy_cfg.bypass.unwrap_or(false);

            let limiter: Arc<dyn RateLimiter> =
                Arc::new(TokenBucket::new(Arc::clone(&store), capacity, refill_rate));
            let extractor: Arc<dyn IdentityExtractor> = Arc::new(IpExtractor);

            let resolved = ResolvedPolicy {
                name: policy_cfg.name.clone(),
                limiter,
                extractor,
                cost,
                bypass,
            };

            policies.push((policy_cfg.match_rule.clone(), resolved));
        }

        let default_limiter: Arc<dyn RateLimiter> = Arc::new(TokenBucket::new(
            Arc::clone(&store),
            defaults.capacity,
            defaults.refill_rate,
        ));
        let default_extractor: Arc<dyn IdentityExtractor> = Arc::new(IpExtractor);

        let default_policy = ResolvedPolicy {
            name: "__default__".to_string(),
            limiter: default_limiter,
            extractor: default_extractor,
            cost: defaults.cost,
            bypass: false,
        };

        Self {
            policies,
            default_policy,
        }
    }

    /// First-match-wins. Falls back to `default_policy` if no policy matches.
    pub fn resolve(&self, path: &str, method: &str) -> &ResolvedPolicy {
        for (rule, resolved) in &self.policies {
            if policy_matches(rule, path, method) {
                return resolved;
            }
        }
        &self.default_policy
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AccessConfig, DefaultsConfig, MatchRule, MemoryStoreConfig, PolicyConfig, ServerConfig,
        StoreConfig,
    };
    use std::net::SocketAddr;
    use std::time::Duration;

    fn make_config(policies: Vec<PolicyConfig>) -> Config {
        Config {
            server: ServerConfig {
                listen: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
                upstream_url: "http://localhost:3000".to_string(),
                proxy: Default::default(),
            },
            defaults: DefaultsConfig {
                algorithm: "token_bucket".to_string(),
                capacity: 100,
                refill_rate: 10.0,
                cost: 1,
                fail_mode: Default::default(),
            },
            store: StoreConfig {
                memory: MemoryStoreConfig {
                    max_entries: 1000,
                    eviction_interval: Duration::from_secs(60),
                },
            },
            access: AccessConfig::default(),
            policy: policies,
        }
    }

    fn make_policy(name: &str, path: &str, methods: Option<Vec<&str>>, bypass: Option<bool>) -> PolicyConfig {
        PolicyConfig {
            name: name.to_string(),
            match_rule: MatchRule {
                path: path.to_string(),
                methods: methods.map(|ms| ms.iter().map(|m| m.to_string()).collect()),
            },
            algorithm: None,
            capacity: None,
            refill_rate: None,
            cost: None,
            identify_by: None,
            bypass,
        }
    }

    fn make_store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new(1000))
    }

    #[test]
    fn resolves_matching_policy() {
        let policies = vec![make_policy("api", "/api/*", None, None)];
        let config = make_config(policies);
        let engine = PolicyEngine::new(&config, make_store());

        let resolved = engine.resolve("/api/users", "GET");
        assert_eq!(resolved.name, "api");
    }

    #[test]
    fn falls_back_to_default() {
        let config = make_config(vec![make_policy("api", "/api/*", None, None)]);
        let engine = PolicyEngine::new(&config, make_store());

        let resolved = engine.resolve("/v2/users", "GET");
        assert_eq!(resolved.name, "__default__");
    }

    #[test]
    fn first_match_wins() {
        let policies = vec![
            make_policy("first", "/api/*", None, None),
            make_policy("second", "/api/users", None, None),
        ];
        let config = make_config(policies);
        let engine = PolicyEngine::new(&config, make_store());

        let resolved = engine.resolve("/api/users", "GET");
        assert_eq!(resolved.name, "first");
    }

    #[test]
    fn bypass_policy() {
        let policies = vec![make_policy("health", "/health", None, Some(true))];
        let config = make_config(policies);
        let engine = PolicyEngine::new(&config, make_store());

        let resolved = engine.resolve("/health", "GET");
        assert_eq!(resolved.name, "health");
        assert!(resolved.bypass);
    }

    #[test]
    fn no_policies_uses_default() {
        let config = make_config(vec![]);
        let engine = PolicyEngine::new(&config, make_store());

        let resolved = engine.resolve("/anything", "POST");
        assert_eq!(resolved.name, "__default__");
        assert!(!resolved.bypass);
        assert_eq!(resolved.cost, 1);
    }
}
