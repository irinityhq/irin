//! TTL + LRU dedup cache for POST /asks.
//!
//! Keyed by `(chat_id, client_msg_id)`. On hit, the entry's LRU position is
//! refreshed. Expired entries are removed on read.

use std::collections::HashMap;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_ENTRIES: usize = 512;
/// PENDING_TTL for the router's pending deliberation / idempotency window (Council P0-1).
/// Must satisfy deliberation_p99 <= LEASE_DURATION (150s in gateway) <= PENDING_TTL .
/// This ensures an in-flight council call's key is still recognized on duplicate arrival,
/// so the router returns 409 (replay) instead of re-invoking the provider (double-bill).
pub const PENDING_TTL_SECS: u64 = 300;
pub const DEFAULT_TTL_SECS: u64 = PENDING_TTL_SECS;

type Key = (String, String);

pub struct Lru<V: Clone> {
    max: usize,
    ttl: Duration,
    // Insertion / access order; back = most recent.
    order: Vec<Key>,
    store: HashMap<Key, (Instant, V)>,
}

impl<V: Clone> Lru<V> {
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            max: max_entries,
            ttl,
            order: Vec::with_capacity(max_entries.min(64)),
            store: HashMap::new(),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, Duration::from_secs(DEFAULT_TTL_SECS))
    }

    pub fn get(&mut self, chat_id: &str, client_msg_id: &str) -> Option<V> {
        let k: Key = (chat_id.to_string(), client_msg_id.to_string());
        let (ts, val) = self.store.get(&k)?.clone();
        if ts.elapsed() > self.ttl {
            self.store.remove(&k);
            self.order.retain(|x| x != &k);
            return None;
        }
        // Move to back (most recent)
        if let Some(pos) = self.order.iter().position(|x| x == &k) {
            self.order.remove(pos);
        }
        self.order.push(k);
        Some(val)
    }

    pub fn put(&mut self, chat_id: &str, client_msg_id: &str, value: V) {
        let k: Key = (chat_id.to_string(), client_msg_id.to_string());
        if let Some(pos) = self.order.iter().position(|x| x == &k) {
            self.order.remove(pos);
        }
        self.store.insert(k.clone(), (Instant::now(), value));
        self.order.push(k);
        while self.order.len() > self.max {
            let old = self.order.remove(0);
            self.store.remove(&old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get() {
        let mut l: Lru<u32> = Lru::with_defaults();
        l.put("c1", "m1", 42);
        assert_eq!(l.get("c1", "m1"), Some(42));
        assert_eq!(l.get("c1", "m2"), None);
    }

    #[test]
    fn ttl_expires() {
        let mut l: Lru<u32> = Lru::new(8, Duration::from_millis(20));
        l.put("c", "m", 1);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(l.get("c", "m"), None);
    }

    #[test]
    fn lru_eviction() {
        let mut l: Lru<u32> = Lru::new(2, Duration::from_secs(60));
        l.put("c", "a", 1);
        l.put("c", "b", 2);
        l.put("c", "c", 3); // should evict "a"
        assert_eq!(l.get("c", "a"), None);
        assert_eq!(l.get("c", "b"), Some(2));
        assert_eq!(l.get("c", "c"), Some(3));
    }
}
