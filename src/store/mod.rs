pub mod memory;

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct BucketState {
    pub tokens: f64,
    pub last_update: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store unavailable: {0}")]
    Unavailable(String),
}

#[async_trait::async_trait]
pub trait StateStore: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Option<BucketState>, StoreError>;
    async fn set(&self, key: &str, state: BucketState, ttl: Duration) -> Result<(), StoreError>;
    async fn increment(&self, key: &str, delta: f64, ttl: Duration) -> Result<f64, StoreError>;
    async fn delete(&self, key: &str) -> Result<(), StoreError>;
    async fn is_healthy(&self) -> bool;
    fn store_name(&self) -> &'static str;
}
