//! Token-bucket rate limiting (`docs/sync-architecture.md`, section 6).
//!
//! Two limiters live in the app state: one per authenticated token, and one
//! per client IP that only *failed* authentications draw from, so a flood of
//! bad tokens gets 429 without ever locking out a valid token behind the same
//! address (for instance a reverse proxy).

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Debug)]
pub struct Limiter<K> {
    capacity: f64,
    per_second: f64,
    max_keys: usize,
    buckets: HashMap<K, Bucket>,
}

impl<K: Hash + Eq + Clone> Limiter<K> {
    pub fn new(capacity: u32, per_second: f64, max_keys: usize) -> Self {
        Limiter {
            capacity: f64::from(capacity),
            per_second,
            max_keys,
            buckets: HashMap::new(),
        }
    }

    /// Spend one unit for `key`; on refusal, how long until one is available.
    pub fn take(&mut self, key: &K, now: Instant) -> Result<(), Duration> {
        if !self.buckets.contains_key(key) && self.buckets.len() >= self.max_keys {
            self.forget_idle(now);
            // Still full of busy keys: refuse new ones rather than grow.
            if self.buckets.len() >= self.max_keys {
                return Err(Duration::from_secs(1));
            }
        }
        let cap = self.capacity;
        let rate = self.per_second;
        let b = self.buckets.entry(key.clone()).or_insert(Bucket {
            tokens: cap,
            last: now,
        });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(cap);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - b.tokens) / rate))
        }
    }

    /// Drop buckets that have refilled completely; they carry no information.
    fn forget_idle(&mut self, now: Instant) {
        let (cap, rate) = (self.capacity, self.per_second);
        self.buckets.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
            b.tokens + elapsed * rate < cap
        });
    }
}

/// Everything the HTTP layer rate limits.
#[derive(Debug)]
pub struct Limits {
    pub per_token: Limiter<String>,
    pub failed_auth: Limiter<std::net::IpAddr>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // A first sync sends 500 ops per request; polling is rare.
            per_token: Limiter::new(120, 10.0, 10_000),
            // Ten bad tokens, then one more every five seconds.
            failed_auth: Limiter::new(10, 0.2, 10_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refills_over_time_and_reports_the_wait() {
        let mut l: Limiter<&str> = Limiter::new(2, 1.0, 10);
        let t0 = Instant::now();
        assert!(l.take(&"a", t0).is_ok());
        assert!(l.take(&"a", t0).is_ok());
        let wait = l.take(&"a", t0).unwrap_err();
        assert!(wait <= Duration::from_secs(1) && wait > Duration::ZERO);
        // Other keys are independent.
        assert!(l.take(&"b", t0).is_ok());
        assert!(l.take(&"a", t0 + Duration::from_secs(1)).is_ok());
        assert!(l.take(&"a", t0 + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn memory_is_bounded() {
        let mut l: Limiter<u32> = Limiter::new(1, 0.001, 3);
        let t0 = Instant::now();
        for k in 0..3 {
            assert!(l.take(&k, t0).is_ok());
        }
        // All three are busy (drained), so a fourth key is refused.
        assert!(l.take(&99, t0).is_err());
        // Much later they have refilled and are forgotten.
        assert!(l.take(&99, t0 + Duration::from_secs(10_000)).is_ok());
    }
}
