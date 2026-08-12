//! Small bounded LRU for recall results (T5.4, spec §8).
//!
//! Keyed by `(query_hash, top_k, traversal_depth, mutation_epoch)`. Epoch
//! invalidation only: any graph mutation — mutation-log writes AND RAM-local
//! reservation transitions ([`crate::graph::Graph::set_reservation`] /
//! [`crate::graph::Graph::clear_reservation`] bump the epoch directly, since
//! no Mutation kind exists for them) — bumps [`crate::graph::Graph::epoch`],
//! so an entry whose key carries a stale epoch is simply a miss. There are no
//! generation counters (the arena is gone).
//!
//! The cache is deliberately plain: no interior mutability, no locks. The
//! integrator decides locking at the call site.
//!
//! Eviction policy: classic LRU via a monotonic access tick. `get` and `insert`
//! stamp the entry with the current tick; insertion at capacity evicts the
//! entry with the smallest tick (least recently used). Eviction scans the map
//! (O(capacity), capacity is small), and only when full, so inserts amortize to
//! O(1). Ticks are u64 and wrap silently; after 2^64 accesses a wrap could only
//! blur ordering between two equal-tick entries, never serve stale data.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::types::RecallResult;

/// Default cache capacity (128 recall results, bounded and small).
///
/// Config wiring is the integrator's: this module const is the single sizing
/// knob until `src/config.rs` grows a recall section.
pub const DEFAULT_CACHE_CAPACITY: usize = 128;

/// Cached payload: the full recall output.
///
/// Type alias for [`RecallResult`] — the [`RecallCache`] default value type.
/// The cache is generic over its value so callers can store the epoch-stable
/// pipeline artifact instead (the recall entry caches phase-1 + expansion and
/// re-renders time-sensitive output on every call).
pub type CacheValue = RecallResult;

/// Cache key: identity of a recall invocation.
///
/// `query_hash` is a deterministic hash of the query string (see
/// [`CacheKey::new`]); the other fields are the recall parameters plus the
/// graph's mutation epoch at answer time.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Deterministic hash of the query string, see [`CacheKey::new`].
    pub query_hash: u64,
    /// Recall `top_k` parameter.
    pub top_k: usize,
    /// Recall `traversal_depth` parameter.
    pub traversal_depth: usize,
    /// `Graph::epoch` at answer time; any mutation invalidates the entry.
    pub mutation_epoch: u64,
}

impl CacheKey {
    /// Build a key, hashing `query` with `DefaultHasher`.
    ///
    /// **Determinism caveat:** `DefaultHasher` (SipHash with fixed keys) is
    /// deterministic for the lifetime of a process and across processes on the
    /// same compiler, but is **not** guaranteed stable across Rust releases
    /// (same caveat as `crate::embed::fixture`). The cache is in-memory per
    /// run, so within-run stability is all that is required.
    pub fn new(query: &str, top_k: usize, traversal_depth: usize, mutation_epoch: u64) -> Self {
        let mut h = DefaultHasher::new();
        query.hash(&mut h);
        Self {
            query_hash: h.finish(),
            top_k,
            traversal_depth,
            mutation_epoch,
        }
    }
}

/// Bounded LRU cache of recall results.
///
/// Not `Sync`-friendly by design; wrap in a lock at the call site.
pub struct RecallCache<V = RecallResult> {
    entries: HashMap<CacheKey, (V, u64)>,
    capacity: usize,
    tick: u64,
}

impl<V> RecallCache<V> {
    /// Create a cache with [`DEFAULT_CACHE_CAPACITY`].
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CACHE_CAPACITY)
    }

    /// Create a cache with the given capacity.
    ///
    /// Panics on a zero capacity: a bounded LRU with no slots is a bug at the
    /// call site, not a supported mode.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "RecallCache capacity must be > 0");
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity,
            tick: 0,
        }
    }

    /// Fetch a cached result, marking the entry most-recently-used on a hit.
    pub fn get(&mut self, key: &CacheKey) -> Option<&V> {
        let tick = self.next_tick();
        if let Some((result, entry_tick)) = self.entries.get_mut(key) {
            *entry_tick = tick;
            Some(result)
        } else {
            None
        }
    }

    /// Store a result. A fresh key at capacity evicts the least-recently-used
    /// entry; an existing key is overwritten and re-stamped.
    pub fn insert(&mut self, key: CacheKey, result: V) {
        let tick = self.next_tick();
        if let Some((slot, entry_tick)) = self.entries.get_mut(&key) {
            *slot = result;
            *entry_tick = tick;
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_lru();
        }
        self.entries.insert(key, (result, tick));
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries are cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of entries before eviction kicks in.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.tick = 0;
    }

    fn next_tick(&mut self) -> u64 {
        let t = self.tick;
        self.tick = self.tick.wrapping_add(1);
        t
    }

    fn evict_lru(&mut self) {
        let mut lru: Option<CacheKey> = None;
        let mut lru_tick = u64::MAX;
        for (key, (_, tick)) in &self.entries {
            if *tick < lru_tick {
                lru_tick = *tick;
                lru = Some(key.clone());
            }
        }
        if let Some(key) = lru {
            self.entries.remove(&key);
        }
    }
}

impl Default for RecallCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(context: &str) -> RecallResult {
        RecallResult {
            hits: Vec::new(),
            context: context.to_string(),
            warnings: Vec::new(),
        }
    }

    fn key(query: &str, top_k: usize, depth: usize, epoch: u64) -> CacheKey {
        CacheKey::new(query, top_k, depth, epoch)
    }

    #[test]
    fn hit_returns_inserted_result() {
        let mut cache = RecallCache::new();
        let k = key("list sessions", 5, 1, 0);
        let r = result("sessions payload");
        cache.insert(k.clone(), r.clone());
        assert_eq!(cache.get(&k), Some(&r));
    }

    #[test]
    fn miss_returns_none_for_unknown_key() {
        let mut cache = RecallCache::new();
        cache.insert(key("known", 5, 1, 0), result("payload"));
        assert_eq!(cache.get(&key("unknown", 5, 1, 0)), None);
        // Same query but different parameters is a different key.
        assert_eq!(cache.get(&key("known", 7, 1, 0)), None);
        assert_eq!(cache.get(&key("known", 5, 2, 0)), None);
    }

    #[test]
    fn insert_at_capacity_evicts_oldest() {
        let mut cache = RecallCache::with_capacity(2);
        let a = key("a", 5, 1, 0);
        let b = key("b", 5, 1, 0);
        let c = key("c", 5, 1, 0);
        cache.insert(a.clone(), result("a"));
        cache.insert(b.clone(), result("b"));
        cache.insert(c.clone(), result("c"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&a), None, "oldest entry must be evicted");
        assert_eq!(cache.get(&b), Some(&result("b")));
        assert_eq!(cache.get(&c), Some(&result("c")));
    }

    #[test]
    fn retouched_entry_survives_eviction() {
        let mut cache = RecallCache::with_capacity(2);
        let a = key("a", 5, 1, 0);
        let b = key("b", 5, 1, 0);
        let c = key("c", 5, 1, 0);
        cache.insert(a.clone(), result("a"));
        cache.insert(b.clone(), result("b"));
        // Re-touch a: now b is least-recently-used.
        assert_eq!(cache.get(&a), Some(&result("a")));
        cache.insert(c.clone(), result("c"));
        assert_eq!(
            cache.get(&a),
            Some(&result("a")),
            "re-touched entry must survive"
        );
        assert_eq!(cache.get(&b), None);
        assert_eq!(cache.get(&c), Some(&result("c")));
    }

    #[test]
    fn epoch_change_invalidates_entry() {
        let mut cache = RecallCache::new();
        // Same query and parameters, mutation_epoch bumped by any mutation.
        let before = key("q", 5, 1, 1);
        let after = key("q", 5, 1, 2);
        cache.insert(before.clone(), result("stale"));
        assert_eq!(cache.get(&before), Some(&result("stale")));
        assert_eq!(cache.get(&after), None, "any epoch bump must miss");
    }

    #[test]
    fn distinct_queries_map_to_distinct_keys() {
        let a = key("alpha", 5, 1, 0);
        let b = key("beta", 5, 1, 0);
        assert_ne!(a, b);
        assert_ne!(a.query_hash, b.query_hash);
    }

    #[test]
    fn clear_empties_cache() {
        let mut cache = RecallCache::new();
        let k = key("q", 5, 1, 0);
        cache.insert(k.clone(), result("x"));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get(&k), None);
    }

    #[test]
    fn insert_overwrites_existing_key() {
        let mut cache = RecallCache::new();
        let k = key("q", 5, 1, 0);
        cache.insert(k.clone(), result("first"));
        cache.insert(k.clone(), result("second"));
        assert_eq!(cache.len(), 1, "same key must not duplicate");
        assert_eq!(cache.get(&k), Some(&result("second")));
    }
}
