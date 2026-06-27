//! # dedup — a bounded O(1) LRU "have I seen this?" set
//!
//! The mesh floods, so every node sees the same packet arrive from multiple neighbors.
//! The relay must forward each one **at most once** (PROTOCOL.md: "Generic O(1) LRU …
//! *not* a bloom filter"). This is the minimal building block: insert-or-was-present in
//! amortized O(1), bounded so a hostile flood can never grow memory without limit (a
//! Bridgefy-class DoS, RISKS.md #4). When full it evicts the oldest key first.
//!
//! It is intentionally generic over the key so the relay can dedup on a blind packet-
//! header identity while the gateway dedups on `txId` — neither needs to read the opaque
//! payload.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// A bounded insertion-ordered LRU set.
///
/// `insert` returns `true` the first time a key is seen and `false` for a duplicate. At
/// capacity the oldest key is evicted, so a key seen long ago may eventually be accepted
/// again — exactly the LRU-with-age-bound behavior the protocol specifies.
pub struct DedupCache<K: Eq + Hash + Clone> {
    capacity: usize,
    set: HashSet<K>,
    order: VecDeque<K>,
}

impl<K: Eq + Hash + Clone> DedupCache<K> {
    /// A cache holding at most `capacity` keys (clamped to at least 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        DedupCache {
            capacity,
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    /// Record `key`, returning `true` if it was **not** already present (a fresh packet
    /// to act on) or `false` if it is a duplicate that should be dropped.
    pub fn insert(&mut self, key: K) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
        true
    }

    /// Whether `key` is currently remembered.
    pub fn contains(&self, key: &K) -> bool {
        self.set.contains(key)
    }

    /// The number of keys currently remembered.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_reports_first_sight_then_duplicate() {
        let mut c = DedupCache::new(8);
        assert!(c.insert(1));
        assert!(!c.insert(1));
        assert!(c.insert(2));
        assert_eq!(c.len(), 2);
        assert!(c.contains(&1));
        assert!(!c.contains(&3));
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut c = DedupCache::new(2);
        assert!(c.insert("a"));
        assert!(c.insert("b"));
        // Inserting "c" evicts the oldest ("a").
        assert!(c.insert("c"));
        assert_eq!(c.len(), 2);
        assert!(!c.contains(&"a"));
        assert!(c.contains(&"b"));
        assert!(c.contains(&"c"));
        // "a" is now treated as fresh again (age-bounded LRU, not a permanent filter).
        assert!(c.insert("a"));
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let mut c = DedupCache::new(0);
        assert!(c.insert(10));
        assert!(!c.insert(10));
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
    }
}
