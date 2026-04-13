use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::task::JoinHandle;

use super::{BucketState, StateStore, StoreError};

#[derive(Clone)]
pub struct MemoryStore {
    buckets: Arc<DashMap<String, BucketState>>,
    max_entries: usize,
}

impl MemoryStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            buckets: Arc::new(DashMap::new()),
            max_entries,
        }
    }

    /// Direct access to the underlying DashMap for shard-level atomic operations.
    pub fn buckets(&self) -> &DashMap<String, BucketState> {
        &self.buckets
    }

    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Spawn a background task that periodically evicts expired and excess entries.
    pub fn start_eviction_task(&self, interval: Duration, ttl: Duration) -> JoinHandle<()> {
        let buckets = Arc::clone(&self.buckets);
        let max_entries = self.max_entries;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;

                let now = Instant::now();

                // Remove entries that have not been updated within `ttl`.
                buckets.retain(|_, state| {
                    now.duration_since(state.last_update) <= ttl
                });

                // If still over capacity, evict the oldest entries.
                if buckets.len() > max_entries {
                    // Collect (key, last_update) so we can sort by age.
                    let mut entries: Vec<(String, Instant)> = buckets
                        .iter()
                        .map(|e| (e.key().clone(), e.value().last_update))
                        .collect();

                    // Sort oldest-first.
                    entries.sort_by_key(|(_, ts)| *ts);

                    let excess = buckets.len().saturating_sub(max_entries);
                    for (key, _) in entries.into_iter().take(excess) {
                        buckets.remove(&key);
                    }
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl StateStore for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<BucketState>, StoreError> {
        Ok(self.buckets.get(key).map(|e| e.value().clone()))
    }

    async fn set(&self, key: &str, state: BucketState, _ttl: Duration) -> Result<(), StoreError> {
        self.buckets.insert(key.to_string(), state);
        Ok(())
    }

    async fn increment(&self, key: &str, delta: f64, _ttl: Duration) -> Result<f64, StoreError> {
        let new_val = match self.buckets.get_mut(key) {
            Some(mut entry) => {
                entry.tokens += delta;
                entry.last_update = Instant::now();
                entry.tokens
            }
            None => {
                let state = BucketState {
                    tokens: delta,
                    last_update: Instant::now(),
                };
                self.buckets.insert(key.to_string(), state);
                delta
            }
        };
        Ok(new_val)
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.buckets.remove(key);
        Ok(())
    }

    async fn is_healthy(&self) -> bool {
        true
    }

    fn store_name(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let store = MemoryStore::new(100);
        let result = store.get("missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_then_get() {
        let store = MemoryStore::new(100);
        let state = BucketState {
            tokens: 42.0,
            last_update: Instant::now(),
        };
        store.set("key1", state, Duration::from_secs(60)).await.unwrap();

        let retrieved = store.get("key1").await.unwrap().unwrap();
        assert_eq!(retrieved.tokens, 42.0);
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let store = MemoryStore::new(100);
        let state = BucketState {
            tokens: 10.0,
            last_update: Instant::now(),
        };
        store.set("key2", state, Duration::from_secs(60)).await.unwrap();
        store.delete("key2").await.unwrap();

        let result = store.get("key2").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn increment_creates_and_adds() {
        let store = MemoryStore::new(100);

        // First increment creates the entry.
        let v1 = store.increment("counter", 5.0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(v1, 5.0);

        // Second increment adds to it.
        let v2 = store.increment("counter", 3.0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(v2, 8.0);
    }

    #[tokio::test]
    async fn is_healthy_returns_true() {
        let store = MemoryStore::new(100);
        assert!(store.is_healthy().await);
    }

    #[tokio::test]
    async fn eviction_removes_expired_entries() {
        let store = MemoryStore::new(100);
        let ttl = Duration::from_secs(60);

        // Insert one "old" entry (last_update 120 seconds ago).
        store.buckets().insert(
            "old".to_string(),
            BucketState {
                tokens: 1.0,
                last_update: Instant::now() - Duration::from_secs(120),
            },
        );

        // Insert one fresh entry.
        store.buckets().insert(
            "new".to_string(),
            BucketState {
                tokens: 1.0,
                last_update: Instant::now(),
            },
        );

        assert_eq!(store.bucket_count(), 2);

        // Manually run the same retain logic used by the eviction task.
        let now = Instant::now();
        store.buckets().retain(|_, state| {
            now.duration_since(state.last_update) <= ttl
        });

        assert_eq!(store.bucket_count(), 1);
        assert!(store.get("new").await.unwrap().is_some());
        assert!(store.get("old").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bucket_count_tracks_entries() {
        let store = MemoryStore::new(100);
        assert_eq!(store.bucket_count(), 0);

        store.set("a", BucketState { tokens: 1.0, last_update: Instant::now() }, Duration::from_secs(60)).await.unwrap();
        assert_eq!(store.bucket_count(), 1);

        store.set("b", BucketState { tokens: 2.0, last_update: Instant::now() }, Duration::from_secs(60)).await.unwrap();
        assert_eq!(store.bucket_count(), 2);

        store.delete("a").await.unwrap();
        assert_eq!(store.bucket_count(), 1);
    }
}
