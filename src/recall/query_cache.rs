//! Per-session LRU of recall **query embeddings** (#14).
//!
//! A query vector is a function of the query text and the embedder that
//! produced it, and of nothing in the graph. So unlike the recall cache
//! ([`super::cache`]), whose key carries the mutation epoch, this cache keys
//! on the exact query text and checks the [`EmbeddingContract`] the vector
//! was embedded under: a write between two identical recalls does not cost
//! the second one its embed, and a vector embedded under one contract is
//! never handed to a reader expecting another.
//!
//! **Only query-role vectors** (#22). Every entry is filled from
//! [`super::candidates::embed_query`], which calls
//! [`Embedder::embed_query`], so the key (query text, contract) names exactly
//! what was embedded: that text, in the query role, under that contract. An
//! adapter whose query role depends on a prompt names its prompt profile in
//! the contract's `model`, so a profile change misses rather than serving a
//! vector from the old prompt. Nothing may insert a document-role vector
//! (`Embedder::embed`) here: for an asymmetric model it is a different vector
//! for the same text. Image queries (recall by image, a later #22 PR) are not
//! cached.
//!
//! **Scope: one per session, inside `Memory`** (#32 decision 13). A
//! process-wide cache keyed by text alone would let one user learn, from
//! reply timing, that another user had run the same query. Do not share an
//! instance across sessions.
//!
//! **Bounded** by entry count *and* bytes ([`QUERY_CACHE_MAX_ENTRIES`],
//! [`QUERY_CACHE_MAX_BYTES`]). An entry's charge is its query text, its
//! vector (`4 * dim`) and its contract's strings, plus
//! [`ENTRY_OVERHEAD_BYTES`] for the map slot and headers. An entry that
//! alone exceeds the byte budget is not cached. Eviction is least recently
//! used, by a monotonic tick, as in [`super::cache::RecallCache`].
//!
//! Plain data, no locks: the owner wraps it in a short, synchronous mutex
//! and never holds that across the embed's `.await` (see
//! `embed_query_cached`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::embed::Embedder;
use crate::store::vector_source::VectorCandidates;
use crate::types::EmbeddingContract;

/// Most entries one session keeps.
pub const QUERY_CACHE_MAX_ENTRIES: usize = 128;

/// Most bytes one session's entries are charged in total (1 MiB). At
/// BGE-M3's 1,024 dimensions a short query costs about 4.4 KiB, so the entry
/// cap binds first (128 entries, about 560 KiB); long queries hit this
/// budget instead (a 16 KiB query, about the MCP tools' published
/// `maxLength`, costs about 20 KiB, so about 50 of them fit). That
/// `maxLength` is a schema hint in characters, not an enforced byte cap; a
/// longer query is simply charged more, and one over the whole budget is
/// not cached.
pub const QUERY_CACHE_MAX_BYTES: usize = 1024 * 1024;

/// Fixed charge per entry for the hash-map slot, the `String`, `Arc<[f32]>`
/// and contract headers, and the tick. Measured on rustc 1.99, a full
/// 128-entry table costs about 226 B per entry (a 112 B slot in 256
/// buckets) plus 16 B per `Arc` header; 256 covers that with allocator
/// rounding to spare. An estimate, not an exact allocator figure.
pub const ENTRY_OVERHEAD_BYTES: usize = 256;

struct Entry {
    contract: EmbeddingContract,
    vector: Arc<[f32]>,
    bytes: usize,
    tick: u64,
}

/// Bounded LRU of query embeddings for one session.
pub struct QueryEmbeddingCache {
    entries: HashMap<String, Entry>,
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    tick: u64,
}

impl Default for QueryEmbeddingCache {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryEmbeddingCache {
    /// A cache with the default bounds.
    pub fn new() -> Self {
        Self::with_limits(QUERY_CACHE_MAX_ENTRIES, QUERY_CACHE_MAX_BYTES)
    }

    /// A cache with explicit bounds. Panics on a zero bound: a cache that can
    /// hold nothing is a bug at the call site.
    pub fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        assert!(
            max_entries > 0 && max_bytes > 0,
            "QueryEmbeddingCache bounds must be > 0"
        );
        Self {
            entries: HashMap::new(),
            max_entries,
            max_bytes,
            bytes: 0,
            tick: 0,
        }
    }

    /// The bytes one entry is charged.
    pub fn entry_bytes(query: &str, contract: &EmbeddingContract, dim: usize) -> usize {
        ENTRY_OVERHEAD_BYTES
            + query.len()
            + dim * std::mem::size_of::<f32>()
            + contract.kind.len()
            + contract.model.as_ref().map_or(0, String::len)
    }

    /// The vector cached for `query` under `contract`, marking it most
    /// recently used. An entry embedded under a different contract is a miss.
    pub fn get(&mut self, query: &str, contract: &EmbeddingContract) -> Option<Arc<[f32]>> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(query)?;
        if &entry.contract != contract {
            return None;
        }
        entry.tick = tick;
        Some(entry.vector.clone())
    }

    /// Cache `vector` for `query` under `contract`, replacing any entry for
    /// the same text and evicting least recently used entries until both
    /// bounds hold. An entry larger than the whole byte budget is dropped.
    pub fn insert(&mut self, query: &str, contract: &EmbeddingContract, vector: Arc<[f32]>) {
        let bytes = Self::entry_bytes(query, contract, vector.len());
        self.remove(query);
        if bytes > self.max_bytes {
            return;
        }
        while self.entries.len() >= self.max_entries || self.bytes + bytes > self.max_bytes {
            if !self.evict_lru() {
                break;
            }
        }
        let tick = self.next_tick();
        self.bytes += bytes;
        self.entries.insert(
            query.to_owned(),
            Entry {
                contract: contract.clone(),
                vector,
                bytes,
                tick,
            },
        );
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes currently charged.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn remove(&mut self, query: &str) {
        if let Some(old) = self.entries.remove(query) {
            self.bytes -= old.bytes;
        }
    }

    fn next_tick(&mut self) -> u64 {
        let t = self.tick;
        self.tick = self.tick.wrapping_add(1);
        t
    }

    /// Evict the least recently used entry; false when empty.
    fn evict_lru(&mut self) -> bool {
        let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.tick)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        self.remove(&key);
        true
    }
}

/// Recall's query embed through the session's cache (#14).
///
/// Same contract as [`super::candidates::embed_query`], which it wraps:
/// `Ok(None)` when the vector leg cannot run (no lookup, no embed), `Err`
/// with the warning line when the embed fails. A hit returns the cached
/// vector without calling the embedder; a miss embeds and caches the result.
/// A failed embed is not cached, so the next recall tries again. Nor is a
/// vector [`cacheable`] rejects: it is still returned, so this recall behaves
/// exactly as it would uncached, but the next one embeds afresh rather than
/// having one bad answer pinned until eviction.
///
/// The lock is taken twice, briefly, and never across the embed's `.await`,
/// so concurrent recalls on one session never wait on each other's embed.
/// Two concurrent misses for the same text both embed; the second insert
/// replaces the first with an equal vector. Not worth a single-flight.
pub(crate) async fn embed_query_cached(
    cache: &Mutex<QueryEmbeddingCache>,
    vectors: VectorCandidates<'_>,
    embedder: &dyn Embedder,
    contract: &EmbeddingContract,
    query: &str,
) -> Result<Option<Arc<[f32]>>, String> {
    if !vectors.available() {
        return Ok(None);
    }
    let cached = cache.lock().get(query, contract);
    if let Some(vector) = cached {
        return Ok(Some(vector));
    }
    let Some(vector) = super::candidates::embed_query(vectors, embedder, query).await? else {
        return Ok(None);
    };
    let vector: Arc<[f32]> = vector.into();
    if cacheable(&vector, contract) {
        cache.lock().insert(query, contract, vector.clone());
    }
    Ok(Some(vector))
}

/// Whether an embedder's answer is fit to keep (#14 review L1): exactly
/// `contract.dim` wide, every value finite, and a non-zero norm. The shipped
/// embedders already refuse anything else, but [`Embedder`] is a public trait
/// and a custom one may not; a transient bad vector should cost one recall,
/// not every repeat of that query.
pub(crate) fn cacheable(vector: &[f32], contract: &EmbeddingContract) -> bool {
    vector.len() == contract.dim
        && vector.iter().all(|x| x.is_finite())
        && vector.iter().any(|&x| x != 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(model: Option<&str>) -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: model.map(str::to_owned),
            dim: 4,
        }
    }

    fn vector(x: f32) -> Arc<[f32]> {
        vec![x; 4].into()
    }

    #[test]
    fn hit_returns_the_inserted_vector() {
        let mut cache = QueryEmbeddingCache::new();
        let c = contract(None);
        cache.insert("user schema", &c, vector(1.0));
        assert_eq!(cache.get("user schema", &c).as_deref(), Some(&[1.0; 4][..]));
        assert_eq!(
            cache.get("user schema ", &c),
            None,
            "exact text, no folding"
        );
        assert_eq!(cache.get("User schema", &c), None, "exact text, no folding");
    }

    #[test]
    fn a_different_contract_is_a_miss_and_insert_replaces_it() {
        let mut cache = QueryEmbeddingCache::new();
        let v1 = contract(Some("model-v1"));
        let v2 = contract(Some("model-v2"));
        cache.insert("q", &v1, vector(1.0));
        assert_eq!(cache.get("q", &v2), None, "never served across contracts");
        cache.insert("q", &v2, vector(2.0));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("q", &v1), None);
        assert_eq!(cache.get("q", &v2).as_deref(), Some(&[2.0; 4][..]));
    }

    #[test]
    fn the_entry_cap_evicts_the_least_recently_used() {
        let mut cache = QueryEmbeddingCache::with_limits(2, usize::MAX);
        let c = contract(None);
        cache.insert("a", &c, vector(1.0));
        cache.insert("b", &c, vector(2.0));
        assert!(cache.get("a", &c).is_some(), "touch a: b is now LRU");
        cache.insert("c", &c, vector(3.0));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b", &c).is_none());
        assert!(cache.get("a", &c).is_some());
        assert!(cache.get("c", &c).is_some());
    }

    #[test]
    fn the_byte_budget_evicts_and_accounting_balances() {
        let c = contract(None);
        let one = QueryEmbeddingCache::entry_bytes("a", &c, 4);
        let mut cache = QueryEmbeddingCache::with_limits(100, one * 2);
        cache.insert("a", &c, vector(1.0));
        cache.insert("b", &c, vector(2.0));
        assert_eq!(cache.bytes(), one * 2);
        cache.insert("c", &c, vector(3.0));
        assert_eq!(cache.len(), 2, "the budget holds two");
        assert_eq!(cache.bytes(), one * 2);
        assert!(cache.get("a", &c).is_none(), "the LRU went");
        // Re-inserting a key replaces its charge rather than adding to it.
        cache.insert("c", &c, vector(4.0));
        assert_eq!(cache.bytes(), one * 2);
        cache.clear();
        assert_eq!((cache.len(), cache.bytes()), (0, 0));
    }

    #[test]
    fn an_entry_larger_than_the_budget_is_not_cached() {
        let c = contract(None);
        let mut cache = QueryEmbeddingCache::with_limits(8, 1_000);
        cache.insert("small", &c, vector(1.0));
        let long = "x".repeat(2_000);
        cache.insert(&long, &c, vector(2.0));
        assert!(cache.get(&long, &c).is_none());
        assert!(
            cache.get("small", &c).is_some(),
            "an oversized insert evicts nothing"
        );
    }

    #[test]
    fn only_a_full_width_finite_non_zero_vector_is_cacheable() {
        let c = contract(None);
        assert!(cacheable(&[0.5, 0.0, -0.5, 0.0], &c));
        assert!(!cacheable(&[0.5; 3], &c), "too narrow");
        assert!(!cacheable(&[0.5; 5], &c), "too wide");
        assert!(!cacheable(&[0.5, f32::NAN, 0.5, 0.5], &c), "NaN");
        assert!(!cacheable(&[0.5, f32::INFINITY, 0.5, 0.5], &c), "inf");
        assert!(!cacheable(&[0.0; 4], &c), "zero norm");
    }

    #[test]
    fn default_bounds_hold_at_bge_width() {
        let bge = EmbeddingContract {
            kind: "candle".into(),
            model: Some("BAAI/bge-m3@main+sha256:0123456789ab".into()),
            dim: 1024,
        };
        let mut cache = QueryEmbeddingCache::new();
        let v: Arc<[f32]> = vec![0.0; 1024].into();
        for i in 0..(QUERY_CACHE_MAX_ENTRIES * 2) {
            cache.insert(&format!("query number {i}"), &bge, v.clone());
        }
        assert_eq!(cache.len(), QUERY_CACHE_MAX_ENTRIES);
        assert!(cache.bytes() <= QUERY_CACHE_MAX_BYTES);
        let long = "y".repeat(16 * 1024);
        for i in 0..200 {
            cache.insert(&format!("{long}{i}"), &bge, v.clone());
        }
        assert!(cache.bytes() <= QUERY_CACHE_MAX_BYTES, "{}", cache.bytes());
        assert!(cache.len() < QUERY_CACHE_MAX_ENTRIES);
    }
}
