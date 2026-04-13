pub mod token_bucket;

use std::time::{Duration, Instant};

use crate::store::StoreError;

#[derive(Debug, Clone)]
pub struct Decision {
    pub allowed: bool,
    pub remaining: u64,
    pub limit: u64,
    pub reset_at: Instant,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub enum LimiterError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync + 'static {
    async fn check(&self, key: &str, cost: u64, peek: bool) -> Result<Decision, LimiterError>;
    fn algorithm_name(&self) -> &'static str;
}
