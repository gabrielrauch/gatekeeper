use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::store::memory::MemoryStore;
use crate::store::BucketState;

use super::{Decision, LimiterError, RateLimiter};

pub struct TokenBucket {
    store: Arc<MemoryStore>,
    capacity: f64,
    refill_rate: f64,
}

impl TokenBucket {
    pub fn new(store: Arc<MemoryStore>, capacity: u64, refill_rate: f64) -> Self {
        Self {
            store,
            capacity: capacity as f64,
            refill_rate,
        }
    }
}

#[async_trait::async_trait]
impl RateLimiter for TokenBucket {
    async fn check(&self, key: &str, cost: u64, peek: bool) -> Result<Decision, LimiterError> {
        let cost_f = cost as f64;
        let now = Instant::now();

        // Use DashMap entry API for shard-level atomicity.
        let decision = self.store.buckets().entry(key.to_string()).or_insert_with(|| {
            BucketState {
                tokens: self.capacity,
                last_update: now,
            }
        });

        // Lazy refill: add tokens proportional to elapsed time.
        let elapsed = now.duration_since(decision.last_update).as_secs_f64();
        let refilled = (decision.tokens + elapsed * self.refill_rate).min(self.capacity);

        let allowed = refilled >= cost_f;

        let (new_tokens, retry_after) = if peek {
            // Peek: do not mutate state.
            (refilled, None)
        } else if allowed {
            let after_consume = refilled - cost_f;
            (after_consume, None)
        } else {
            // Denied: still update the refill timestamp but don't consume.
            let retry = if self.refill_rate > 0.0 {
                let deficit = cost_f - refilled;
                Some(Duration::from_secs_f64(deficit / self.refill_rate))
            } else {
                None
            };
            (refilled, retry)
        };

        // Commit state unless peeking.
        if !peek {
            let mut state = decision;
            state.tokens = new_tokens;
            state.last_update = now;
        } else {
            // Drop reference without mutating.
            drop(decision);
        }

        let remaining = new_tokens.floor().max(0.0) as u64;
        let reset_at = if self.refill_rate > 0.0 {
            let time_to_full = (self.capacity - new_tokens) / self.refill_rate;
            now + Duration::from_secs_f64(time_to_full.max(0.0))
        } else {
            now
        };

        Ok(Decision {
            allowed,
            remaining,
            limit: self.capacity as u64,
            reset_at,
            retry_after,
        })
    }

    fn algorithm_name(&self) -> &'static str {
        "token_bucket"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::TokenBucket;
    use crate::algorithm::RateLimiter;
    use crate::store::memory::MemoryStore;

    fn make_limiter(capacity: u64, refill_rate: f64) -> TokenBucket {
        let store = Arc::new(MemoryStore::new(1000));
        TokenBucket::new(store, capacity, refill_rate)
    }

    #[tokio::test]
    async fn allows_request_within_capacity() {
        let lb = make_limiter(10, 1.0);
        let d = lb.check("k", 1, false).await.unwrap();
        assert!(d.allowed);
        assert_eq!(d.remaining, 9);
        assert_eq!(d.limit, 10);
    }

    #[tokio::test]
    async fn denies_when_exhausted() {
        let lb = make_limiter(3, 0.0); // no refill so we can exhaust deterministically
        assert!(lb.check("k", 1, false).await.unwrap().allowed);
        assert!(lb.check("k", 1, false).await.unwrap().allowed);
        assert!(lb.check("k", 1, false).await.unwrap().allowed);
        let d = lb.check("k", 1, false).await.unwrap();
        assert!(!d.allowed);
    }

    #[tokio::test]
    async fn different_keys_are_independent() {
        let lb = make_limiter(1, 0.0);
        assert!(lb.check("a", 1, false).await.unwrap().allowed);
        let d = lb.check("a", 1, false).await.unwrap();
        assert!(!d.allowed);

        // Key "b" has its own full bucket.
        assert!(lb.check("b", 1, false).await.unwrap().allowed);
    }

    #[tokio::test]
    async fn cost_greater_than_one() {
        let lb = make_limiter(10, 0.0);
        let d1 = lb.check("k", 5, false).await.unwrap();
        assert!(d1.allowed);
        assert_eq!(d1.remaining, 5);

        let d2 = lb.check("k", 6, false).await.unwrap();
        assert!(!d2.allowed);
    }

    #[tokio::test]
    async fn refills_over_time() {
        let lb = make_limiter(10, 100.0); // 100 tokens/sec

        // Exhaust the bucket.
        let d = lb.check("k", 10, false).await.unwrap();
        assert!(d.allowed);
        assert_eq!(d.remaining, 0);

        // Wait ~110 ms → ~11 tokens refilled, capped at 10.
        tokio::time::sleep(Duration::from_millis(110)).await;

        let d2 = lb.check("k", 1, false).await.unwrap();
        assert!(d2.allowed);
    }

    #[tokio::test]
    async fn peek_does_not_consume() {
        let lb = make_limiter(5, 0.0);

        let d1 = lb.check("k", 1, true).await.unwrap();
        let d2 = lb.check("k", 1, true).await.unwrap();

        assert!(d1.allowed);
        assert!(d2.allowed);
        // Both peeks should report the same remaining (5).
        assert_eq!(d1.remaining, 5);
        assert_eq!(d2.remaining, 5);
    }

    #[tokio::test]
    async fn retry_after_is_reasonable() {
        let lb = make_limiter(10, 10.0); // 10 tokens/sec → 0.1s per token

        // Exhaust bucket.
        lb.check("k", 10, false).await.unwrap();

        // Request 1 more — should be denied.
        let d = lb.check("k", 1, false).await.unwrap();
        assert!(!d.allowed);

        let retry = d.retry_after.unwrap();
        // Should be approximately 0.1s (±50ms tolerance).
        assert!(retry.as_secs_f64() < 0.15, "retry_after too large: {retry:?}");
        assert!(retry.as_secs_f64() > 0.0, "retry_after should be > 0");
    }

    #[tokio::test]
    async fn capacity_caps_refill() {
        let lb = make_limiter(5, 10_000.0); // very fast refill

        // Consume some tokens.
        lb.check("k", 3, false).await.unwrap();

        // Even with very fast refill, remaining should not exceed capacity.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let d = lb.check("k", 1, false).await.unwrap();
        assert!(d.allowed);
        assert!(d.remaining <= 5, "remaining {} exceeds capacity 5", d.remaining);
    }
}
