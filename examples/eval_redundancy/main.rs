//! #181: redundancy-aware selection on the `fixtures/eval/` set.
//!
//! Four stores are derived through `Memory` (SQLite, one concept per
//! derive, each self-framed as a one-concept `lambo_derive` stores it):
//!
//! * `clean`: the 100 corpus concepts, nothing injected (the control);
//! * `dup`: the corpus plus, for every corpus concept with a labelled
//!   paraphrase, an exact copy of its text under another concept type (the
//!   efficacy run's Entity/Observation shape) and every labelled rewording;
//! * `contrast`: the corpus plus every labelled related-but-distinct partner
//!   of a corpus concept (polarity, exception, correction, repository,
//!   identifier, quantity, topic-neighbour), with no declared contradiction;
//! * `contrast-declared`: the same partners, each derived `against` its
//!   anchor (a `Contradicts` edge).
//!
//! For each answerable query the shared window is the session's own top 20
//! with the rule off (`top_k = 20`, a budget nothing reaches). Every arm
//! selects `k` hits from the first `min(20, 4k)` of that window, so all arms
//! see the same candidates, scores and order:
//!
//! * `current`: the first `k`;
//! * `content`: rule (a) with the same-content signal only (the issue's
//!   exact-duplicate control);
//! * `a@T`: rule (a), skip-if-redundant, signals same content, `Semantic`
//!   edge and cosine >= `T`, with the `Contradicts` and text vetoes; `a@0.85`
//!   is what ships under BGE-M3 (its merge threshold);
//! * `mmr@L`: maximal marginal relevance, `L · rel − (1 − L) · max sim`,
//!   `rel` the final score over the window's best, `sim` 1 for the same
//!   content else the cosine (0 without two vectors);
//! * `fl`: facility location (PACMS, arXiv:2606.20047, section 3.1),
//!   `w_ij = rel(i) · max(0, cos(e_i, e_j))`, `F(S) = Σ_i max_{j∈S} w_ij`
//!   over the window, greedy.
//!
//! `mmr` and `fl` are gated like rule (a): when rule (a)'s detector finds no
//! redundant pair in the window they return `current`. They are offline
//! comparisons only; rule (a) is the shipped code path, and the harness
//! checks its offline replay against the real pipeline with the rule on
//! (`validation` in the output).
//!
//! The rendered context is each selected hit's block as recall renders it
//! (`also <id>` items on rule (a)'s kept hits), joined by blank lines and
//! cut to the budget by the longest whole-block prefix, measured with
//! recall's default estimator. A facet is covered when a rendered hit is a
//! grade-2 concept of the query, a grade-1 concept whose text holds at
//! least 80% of the facet's content words, or a copy (exact or reworded)
//! of either; a contrast partner covers nothing.
//!
//! ```text
//! HF_HUB_OFFLINE=1 LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
//!   cargo run --release --features embed-candle,store-sqlite --example eval_redundancy -- \
//!   --cache /path/to/bge-m3.jsonl --out evidence/issue-181/runs/bge-m3-candle-cpu.json
//! ```

#[path = "../eval_measure/schema.rs"]
#[allow(dead_code)]
mod schema;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lambo::embed::{EmbedError, EmbedderConfig};
use lambo::graph::derive::ParentOf;
use lambo::memory::Memory;
use lambo::recall::redundancy::{distinct_claims, normalized_content, window, RecallSelection};
use lambo::{
    AgentId, ConceptCitations, ConceptType, EdgeType, Embedder, LamboFile, NodeId, RecallQuery,
    SessionId,
};
use serde_json::{json, Value};

const SESSION: &str = "eval-redundancy";
const WINDOW_K: usize = 20;
const KS: [usize; 2] = [3, 5];
const BUDGETS: [usize; 2] = [250, 500];
const THRESHOLDS: [f64; 3] = [0.85, 0.90, 0.95];
const LAMBDAS: [f64; 5] = [0.5, 0.6, 0.7, 0.8, 0.9];
/// The shipped arm: rule (a) at BGE-M3's merge threshold.
const SHIPPED: f64 = 0.85;

// ---------------------------------------------------------------------------
// The embedding cache (eval_measure's format and key, as eval_fusion's).
// ---------------------------------------------------------------------------

struct CachedEmbedder {
    inner: Arc<dyn Embedder>,
    contract: String,
    stored: HashMap<String, Vec<f32>>,
    fresh: Mutex<HashMap<String, Vec<f32>>>,
    file: Option<Mutex<std::fs::File>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

impl CachedEmbedder {
    fn key(&self, role: &str, text: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [self.contract.as_str(), role, text] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn lookup(&self, role: &str, text: &str) -> Option<Vec<f32>> {
        let key = self.key(role, text);
        let v = self
            .stored
            .get(&key)
            .cloned()
            .or_else(|| self.fresh.lock().expect("cache").get(&key).cloned());
        let counter = if v.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        v
    }

    fn keep(&self, role: &str, text: &str, v: &[f32]) {
        use std::io::Write;
        let key = self.key(role, text);
        self.fresh
            .lock()
            .expect("cache")
            .insert(key.clone(), v.to_vec());
        if let Some(f) = &self.file {
            let _ = writeln!(
                f.lock().expect("cache file"),
                "{}",
                json!({"key": key, "v": v})
            );
        }
    }
}

#[async_trait]
impl Embedder for CachedEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(v) = self.lookup("doc", text) {
            return Ok(v);
        }
        let v = self.inner.embed(text).await?;
        self.keep("doc", text, &v);
        Ok(v)
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(v) = self.lookup("query", text) {
            return Ok(v);
        }
        let v = self.inner.embed_query(text).await?;
        self.keep("query", text, &v);
        Ok(v)
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// An `Arc<dyn Embedder>` as a `Box<dyn Embedder>`.
struct Shared(Arc<dyn Embedder>);

#[async_trait]
impl Embedder for Shared {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.0.embed_query(text).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

// ---------------------------------------------------------------------------
// Stores
// ---------------------------------------------------------------------------

/// What a derived concept is, relative to the corpus.
#[derive(Clone, Debug)]
enum Role {
    Corpus(String),
    /// The same text as a corpus concept, under another type.
    Exact(String),
    /// A labelled rewording of a corpus concept.
    Reworded(String),
    /// A labelled related-but-distinct partner: anchor, family, pair id.
    Partner {
        anchor: String,
        family: String,
        pair: String,
    },
}

impl Role {
    /// The corpus concept whose facts this one carries, if any.
    fn carries(&self) -> Option<&str> {
        match self {
            Role::Corpus(c) | Role::Exact(c) | Role::Reworded(c) => Some(c),
            Role::Partner { .. } => None,
        }
    }
    fn anchor(&self) -> &str {
        match self {
            Role::Corpus(c) | Role::Exact(c) | Role::Reworded(c) => c,
            Role::Partner { anchor, .. } => anchor,
        }
    }
}

/// One derived store: its database, what each node is, and the graph facts
/// the offline arms read (content, vectors, edges).
struct Store {
    db: PathBuf,
    role: HashMap<NodeId, Role>,
    content: HashMap<NodeId, String>,
    vector: HashMap<NodeId, Vec<f32>>,
    semantic: HashSet<(NodeId, NodeId)>,
    contradicts: HashSet<(NodeId, NodeId)>,
}

fn concept_type(kind: &str) -> Result<ConceptType, String> {
    Ok(match kind {
        "Entity" => ConceptType::Entity,
        "Logic" => ConceptType::Logic,
        "Constraint" => ConceptType::Constraint,
        "Resource" => ConceptType::Resource,
        "Observation" => ConceptType::Observation,
        other => return Err(format!("unknown concept type {other}")),
    })
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.into()).to_string()
}

async fn backends(db: &Path) -> Result<lambo::resolve::ResolvedBackends, String> {
    let toml = format!(
        "[store]\nkind = \"sqlite\"\npath = {}\n",
        toml_string(&db.display().to_string())
    );
    let mut file = LamboFile::from_toml_str(&toml).map_err(|e| e.to_string())?;
    file.embedder = file.embedder.overlay_env().map_err(|e| e.to_string())?;
    let resolved = lambo::resolve::resolve_backends(file).map_err(|e| e.to_string())?;
    resolved
        .store
        .init_schema()
        .await
        .map_err(|e| format!("init_schema: {e}"))?;
    Ok(resolved)
}

async fn open(db: &Path, embedder: Arc<dyn Embedder>, skip: bool) -> Result<Memory, String> {
    let mut resolved = backends(db).await?;
    resolved.embedder = Box::new(Shared(embedder));
    let mut config = resolved.config.clone();
    config.recall_selection = RecallSelection {
        skip_redundant: skip,
        similar_signals: skip,
    };
    // An hour's tick: the warm-up cycle scores the session, and no later
    // cycle runs mid-measurement.
    config.daemon_tick_interval = Duration::from_secs(3600);
    Memory::builder()
        .session(SESSION)
        .agent("eval")
        .backends(resolved)
        .config(config)
        .build()
        .await
        .map_err(|e| e.to_string())
}

/// A text to derive: content, type, role, and the corpus concept it is
/// declared `against`, if any.
struct Plan {
    text: String,
    ty: ConceptType,
    role: Role,
    against: Option<String>,
}

fn plans(set: &schema::EvalSet, condition: &str) -> Result<Vec<Plan>, String> {
    let corpus: HashMap<&str, &schema::Concept> =
        set.corpus.iter().map(|c| (c.id.as_str(), c)).collect();
    let anchored = |p: &schema::Pair| -> Option<&schema::Concept> {
        let c = corpus.get(p.concepts.first()?.as_str())?;
        (c.text == p.a && p.review.is_none()).then_some(*c)
    };
    let mut before = Vec::new();
    let mut after = Vec::new();
    if condition == "dup" {
        let mut copied = HashSet::new();
        for p in set.pairs.iter().filter(|p| p.label == "paraphrase") {
            let Some(c) = anchored(p) else { continue };
            if copied.insert(c.id.clone()) {
                // The efficacy shape. The canonical-key matcher skips
                // Observations, so an Observation copy is written before
                // its anchor; an Observation anchor gets an Entity copy
                // after it.
                let copy = Plan {
                    text: c.text.clone(),
                    ty: if c.kind == "Observation" {
                        ConceptType::Entity
                    } else {
                        ConceptType::Observation
                    },
                    role: Role::Exact(c.id.clone()),
                    against: None,
                };
                if c.kind == "Observation" {
                    after.push(copy);
                } else {
                    before.push(copy);
                }
            }
            after.push(Plan {
                text: p.b.clone(),
                ty: concept_type(&c.kind)?,
                role: Role::Reworded(c.id.clone()),
                against: None,
            });
        }
    }
    if condition.starts_with("contrast") {
        for p in set.pairs.iter().filter(|p| p.label == "related_distinct") {
            let Some(c) = anchored(p) else { continue };
            after.push(Plan {
                text: p.b.clone(),
                ty: concept_type(&c.kind)?,
                role: Role::Partner {
                    anchor: c.id.clone(),
                    family: p.family.clone(),
                    pair: p.id.clone(),
                },
                against: (condition == "contrast-declared").then(|| c.id.clone()),
            });
        }
    }
    let mut out = before;
    for c in &set.corpus {
        out.push(Plan {
            text: c.text.clone(),
            ty: concept_type(&c.kind)?,
            role: Role::Corpus(c.id.clone()),
            against: None,
        });
    }
    out.extend(after);
    Ok(out)
}

async fn derive_store(
    set: &schema::EvalSet,
    condition: &str,
    work: &Path,
    embedder: Arc<dyn Embedder>,
) -> Result<Store, String> {
    let db = work.join(format!("{condition}.db"));
    let mut role = HashMap::new();
    let mut node_of: HashMap<String, NodeId> = HashMap::new();
    {
        let mem = open(&db, embedder.clone(), false).await?;
        let agent = AgentId::from("eval");
        for plan in plans(set, condition)? {
            let citations: Vec<ConceptCitations> = match &plan.against {
                Some(anchor) => vec![ConceptCitations {
                    content: plan.text.clone(),
                    depends_on: Vec::new(),
                    against: vec![node_of[anchor].0.to_string()],
                }],
                None => Vec::new(),
            };
            let out = mem
                .derive_cited_as(
                    &agent,
                    &[(plan.text.as_str(), plan.ty)],
                    &ParentOf::none(),
                    &citations,
                )
                .await
                .map_err(|e| format!("{condition}: deriving {:?}: {e}", plan.role))?;
            let Some(id) = out.created.first().copied() else {
                // A rewording whose canonical key equals its anchor's
                // matched it: it adds no concept, so it is no copy.
                eprintln!("{condition}: {:?} matched an existing concept", plan.role);
                continue;
            };
            if let Role::Corpus(c) = &plan.role {
                node_of.insert(c.clone(), id);
            }
            role.insert(id, plan.role);
            // Distinct created_at, so the recent leg is the same every run.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        mem.close().await.map_err(|e| e.to_string())?;
    }
    let resolved = backends(&db).await?;
    let snap = resolved
        .store
        .load_session(&SessionId::from(SESSION))
        .await
        .map_err(|e| format!("loading {condition}: {e}"))?;
    let mut store = Store {
        db,
        role,
        content: HashMap::new(),
        vector: HashMap::new(),
        semantic: HashSet::new(),
        contradicts: HashSet::new(),
    };
    for c in snap.concepts {
        if let Some(v) = c.embedding {
            store.vector.insert(c.id, v);
        }
        store.content.insert(c.id, c.content);
    }
    for e in snap.edges {
        match e.edge_type {
            EdgeType::Semantic => {
                store.semantic.insert((e.source, e.target));
            }
            EdgeType::Contradicts => {
                store.contradicts.insert((e.source, e.target));
            }
            _ => {}
        }
    }
    Ok(store)
}

// ---------------------------------------------------------------------------
// Selection arms
// ---------------------------------------------------------------------------

/// One window member.
#[derive(Clone, Debug)]
struct Member {
    id: NodeId,
    score: f64,
    content: String,
    ty: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    SameContent,
    SemanticEdge,
    Cosine,
}

impl Store {
    fn cosine(&self, a: NodeId, b: NodeId) -> Option<f64> {
        match (self.vector.get(&a), self.vector.get(&b)) {
            (Some(x), Some(y)) if x.len() == y.len() && !x.is_empty() => {
                Some(f64::from(lambo::embed::cosine(x, y)))
            }
            _ => None,
        }
    }

    fn joined(set: &HashSet<(NodeId, NodeId)>, a: NodeId, b: NodeId) -> bool {
        set.contains(&(a, b)) || set.contains(&(b, a))
    }

    /// Rule (a)'s test, as `recall::redundancy` runs it. `None` threshold:
    /// the same-content signal only.
    fn redundant(&self, a: &Member, b: &Member, threshold: Option<f64>) -> Option<Signal> {
        let signal = if normalized_content(&a.content) == normalized_content(&b.content) {
            Signal::SameContent
        } else if threshold.is_some() && Self::joined(&self.semantic, a.id, b.id) {
            Signal::SemanticEdge
        } else if let Some(t) = threshold
            && self.cosine(a.id, b.id).is_some_and(|c| c >= t)
        {
            Signal::Cosine
        } else {
            return None;
        };
        if Self::joined(&self.contradicts, a.id, b.id) || distinct_claims(&a.content, &b.content) {
            return None;
        }
        Some(signal)
    }

    /// `sim` for MMR: 1 for the same content, else the cosine (0 without
    /// two vectors).
    fn sim(&self, a: &Member, b: &Member) -> f64 {
        if normalized_content(&a.content) == normalized_content(&b.content) {
            1.0
        } else {
            self.cosine(a.id, b.id).unwrap_or(0.0)
        }
    }
}

/// A selection: the chosen members in rendered order, and for rule (a)
/// what each kept member skipped (with the signal).
#[derive(Clone, Debug, Default)]
struct Selection {
    picked: Vec<usize>,
    skipped: Vec<(usize, usize, Signal)>,
}

fn current(w: &[Member], k: usize) -> Selection {
    Selection {
        picked: (0..k.min(w.len())).collect(),
        skipped: Vec::new(),
    }
}

/// Rule (a): walk the window, skip a member redundant with one taken.
fn rule_a(store: &Store, w: &[Member], k: usize, threshold: Option<f64>) -> Selection {
    let win = window(k).min(w.len());
    let mut s = Selection::default();
    for i in 0..w.len() {
        if s.picked.len() >= k {
            break;
        }
        if i < win
            && let Some((kept, signal)) = s
                .picked
                .iter()
                .find_map(|&t| store.redundant(&w[t], &w[i], threshold).map(|g| (t, g)))
        {
            s.skipped.push((kept, i, signal));
            continue;
        }
        s.picked.push(i);
    }
    s
}

/// Whether rule (a)'s detector finds any redundant pair in the window.
fn gate(store: &Store, w: &[Member], k: usize) -> bool {
    let win = window(k).min(w.len());
    (0..win).any(|i| (0..i).any(|j| store.redundant(&w[j], &w[i], Some(SHIPPED)).is_some()))
}

fn mmr(store: &Store, w: &[Member], k: usize, lambda: f64) -> Selection {
    if !gate(store, w, k) {
        return current(w, k);
    }
    let win = window(k).min(w.len());
    let top = w[..win]
        .iter()
        .map(|m| m.score)
        .fold(f64::MIN, f64::max)
        .max(1e-12);
    let mut picked: Vec<usize> = Vec::new();
    while picked.len() < k.min(win) {
        let next = (0..win)
            .filter(|i| !picked.contains(i))
            .map(|i| {
                let rel = w[i].score / top;
                let red = picked
                    .iter()
                    .map(|&j| store.sim(&w[i], &w[j]))
                    .fold(0.0, f64::max);
                (i, lambda * rel - (1.0 - lambda) * red)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(i, _)| i);
        match next {
            Some(i) => picked.push(i),
            None => break,
        }
    }
    picked.sort_unstable();
    Selection {
        picked,
        skipped: Vec::new(),
    }
}

fn facility_location(store: &Store, w: &[Member], k: usize) -> Selection {
    if !gate(store, w, k) {
        return current(w, k);
    }
    let win = window(k).min(w.len());
    // w_ij = rel(i) · max(0, cos(e_i, e_j)), rel on the covered item i.
    let weight = |i: usize, j: usize| -> f64 {
        let cos = if i == j {
            1.0
        } else {
            store.sim(&w[i], &w[j]).max(0.0)
        };
        w[i].score.max(0.0) * cos
    };
    let mut best = vec![0.0f64; win];
    let mut picked: Vec<usize> = Vec::new();
    while picked.len() < k.min(win) {
        let next = (0..win)
            .filter(|j| !picked.contains(j))
            .map(|j| {
                let gain: f64 = (0..win).map(|i| (weight(i, j) - best[i]).max(0.0)).sum();
                (j, gain)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(j, _)| j);
        let Some(j) = next else { break };
        for (i, b) in best.iter_mut().enumerate() {
            *b = b.max(weight(i, j));
        }
        picked.push(j);
    }
    picked.sort_unstable();
    Selection {
        picked,
        skipped: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Rendering and coverage
// ---------------------------------------------------------------------------

fn short_id(id: NodeId) -> String {
    id.0.simple().to_string()[..8].to_string()
}

fn tokens(s: &str) -> usize {
    (s.len() as f64 / 3.5).ceil() as usize
}

/// The rendered context of a selection under `budget`: each block as
/// recall renders it (with rule (a)'s `also` items), the longest prefix of
/// whole blocks that fits. Returns the indices rendered and the text.
fn render(w: &[Member], s: &Selection, budget: usize) -> (Vec<usize>, String) {
    let mut out = String::new();
    let mut shown = Vec::new();
    for &i in &s.picked {
        let skipped: Vec<NodeId> = s
            .skipped
            .iter()
            .filter(|(kept, _, _)| *kept == i)
            .map(|(_, j, _)| w[*j].id)
            .collect();
        let mut meta: Vec<String> = skipped
            .iter()
            .take(3)
            .map(|id| format!("also {}", short_id(*id)))
            .collect();
        if skipped.len() > 3 {
            meta.push(format!("+{} more", skipped.len() - 3));
        }
        let extra = if meta.is_empty() {
            String::new()
        } else {
            format!(", {}", meta.join(", "))
        };
        let block = format!(
            "{} [{}] (score {:.2}, id {}{extra})",
            w[i].content,
            w[i].ty,
            w[i].score,
            short_id(w[i].id)
        );
        let next = if out.is_empty() {
            block
        } else {
            format!("{out}\n\n{block}")
        };
        if tokens(&next) > budget {
            break;
        }
        out = next;
        shown.push(i);
    }
    (shown, out)
}

/// Lowercase alphanumeric words, stopwords dropped, cut to five characters
/// (a crude stem), digits kept whole.
fn content_words(s: &str) -> Vec<String> {
    const STOP: [&str; 24] = [
        "the", "a", "an", "of", "to", "in", "on", "for", "and", "or", "is", "are", "was", "it",
        "its", "be", "by", "at", "as", "with", "that", "this", "from", "not",
    ];
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOP.contains(w))
        .map(|w| {
            if w.chars().any(|c| c.is_ascii_digit()) {
                w.to_string()
            } else {
                w.chars().take(5).collect()
            }
        })
        .collect()
}

/// The facets of `q` that corpus concept `c` covers.
fn covers(q: &schema::Query, c: &schema::Concept) -> Vec<usize> {
    let grade = q
        .relevant
        .iter()
        .find(|r| r.concept == c.id)
        .map_or(0, |r| r.grade);
    match grade {
        2 => (0..q.facets.len()).collect(),
        1 => {
            let have: HashSet<String> = content_words(&c.text).into_iter().collect();
            q.facets
                .iter()
                .enumerate()
                .filter(|(_, f)| {
                    let want = content_words(f);
                    !want.is_empty()
                        && want.iter().filter(|w| have.contains(*w)).count() * 5 >= want.len() * 4
                })
                .map(|(i, _)| i)
                .collect()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Acc {
    n: usize,
    facets: f64,
    facet_hits: usize,
    facet_total: usize,
    distinct: usize,
    rendered: usize,
    tokens: usize,
    changed: usize,
    first_relevant: usize,
}

impl Acc {
    fn json(&self) -> Value {
        let n = self.n.max(1) as f64;
        json!({
            "n": self.n,
            "facet_coverage": r4(self.facets / n),
            "facets_covered": self.facet_hits,
            "facets_total": self.facet_total,
            "distinct_memories_rendered": r4(self.distinct as f64 / n),
            "hits_rendered": r4(self.rendered as f64 / n),
            "tokens": r4(self.tokens as f64 / n),
            "queries_changed": self.changed,
            "grade2_rendered": r4(self.first_relevant as f64 / n),
        })
    }
}

/// Suppression counts per family: windows where the arm drops a member
/// that `current` renders while it renders the member's labelled partner.
#[derive(Default, Clone)]
struct Suppression {
    both_in_current: usize,
    suppressed: usize,
    /// The labelled pairs suppressed, by pair id and query id.
    pairs: Vec<String>,
}

fn r4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

struct Args {
    cache: Option<PathBuf>,
    out: Option<PathBuf>,
    conditions: Vec<String>,
    allow_fixture: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        cache: None,
        out: None,
        conditions: Vec::new(),
        allow_fixture: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cache" => a.cache = Some(it.next().ok_or("--cache needs a path")?.into()),
            "--out" => a.out = Some(it.next().ok_or("--out needs a path")?.into()),
            "--condition" => a
                .conditions
                .push(it.next().ok_or("--condition needs a name")?),
            "--allow-fixture" => a.allow_fixture = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.conditions.is_empty() {
        a.conditions = ["clean", "dup", "contrast", "contrast-declared"]
            .map(String::from)
            .to_vec();
    }
    Ok(a)
}

fn tempdir() -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "lambo-eval-redundancy-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("eval_redundancy: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval");
    let set = schema::load(&dir)?;
    let problems = schema::validate(&set, lambo::surface::limits::MAX_CONTENT_BYTES);
    if !problems.is_empty() {
        return Err(format!("the eval set fails its schema: {problems:?}"));
    }
    let cfg = EmbedderConfig::from_env().map_err(|e| e.to_string())?;
    let kind = format!("{:?}", cfg.kind);
    if kind.to_ascii_lowercase().contains("fixture") && !args.allow_fixture {
        return Err("LAMBO_EMBEDDER is the fixture embedder; pass --allow-fixture".into());
    }
    let inner: Arc<dyn Embedder> =
        Arc::from(lambo::embed::build_embedder(cfg.clone()).map_err(|e| e.to_string())?);
    let identity = lambo::embed::candle_identity(inner.as_ref());
    let model = json!({
        "dim": cfg.dim, "llama_model": cfg.llama_model, "device": cfg.device,
        "repo": cfg.repo, "revision": cfg.revision, "weights_file": cfg.weights_file,
        "gemini_model": cfg.gemini_model,
    });
    let mut cached = CachedEmbedder {
        inner,
        contract: format!(
            "{kind}|{model}|{:?}|{:?}|{:?}|{:?}|{identity:?}",
            cfg.llama_url, cfg.weights_dir, cfg.gemini_project, cfg.gemini_location
        ),
        stored: HashMap::new(),
        fresh: Mutex::new(HashMap::new()),
        file: None,
        hits: AtomicUsize::new(0),
        misses: AtomicUsize::new(0),
    };
    if let Some(path) = &args.cache {
        use std::io::BufRead;
        let dim = cached.inner.dimensions();
        if let Ok(f) = std::fs::File::open(path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && let (Some(k), Some(vs)) = (v["key"].as_str(), v["v"].as_array())
                    && vs.len() == dim
                {
                    let vec: Option<Vec<f32>> =
                        vs.iter().map(|x| x.as_f64().map(|x| x as f32)).collect();
                    if let Some(vec) = vec {
                        cached.stored.insert(k.to_string(), vec);
                    }
                }
            }
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("opening the cache: {e}"))?;
        cached.file = Some(Mutex::new(f));
    }
    let embedder: Arc<CachedEmbedder> = Arc::new(cached);
    let as_dyn: Arc<dyn Embedder> = embedder.clone();
    let corpus: HashMap<&str, &schema::Concept> =
        set.corpus.iter().map(|c| (c.id.as_str(), c)).collect();
    let queries: Vec<&schema::Query> = set
        .queries
        .iter()
        .filter(|q| q.review.is_none() && q.kind == "answerable")
        .collect();

    let mut arms: Vec<String> = vec!["current".into(), "content".into()];
    arms.extend(THRESHOLDS.iter().map(|t| format!("a@{t:.2}")));
    arms.extend(LAMBDAS.iter().map(|l| format!("mmr@{l:.1}")));
    arms.push("fl".into());

    let work = tempdir()?;
    let mut report = BTreeMap::new();
    let mut lines: Vec<String> = Vec::new();
    for condition in &args.conditions {
        let started = Instant::now();
        let store = derive_store(&set, condition, &work, as_dyn.clone()).await?;
        eprintln!(
            "{condition}: derived {} concepts ({} semantic, {} contradicts edges) in {:.0}s",
            store.role.len(),
            store.semantic.len(),
            store.contradicts.len(),
            started.elapsed().as_secs_f64()
        );
        // Two copies of the derived session, opened together: the windows
        // come from one with the rule off, the validation from the other
        // with it on. Same rows and the same warm-up, and reads never wake
        // the daemon, so the two rank alike (recency differs only by the
        // seconds between the two warm-ups).
        let copy = |tag: &str| -> Result<PathBuf, String> {
            let to = work.join(format!("{condition}-{tag}.db"));
            for suffix in ["", "-wal", "-shm"] {
                let from = PathBuf::from(format!("{}{suffix}", store.db.display()));
                if from.exists() {
                    std::fs::copy(&from, format!("{}{suffix}", to.display()))
                        .map_err(|e| format!("copying the session: {e}"))?;
                }
            }
            Ok(to)
        };
        let (off_db, on_db) = (copy("off")?, copy("on")?);
        let off = open(&off_db, as_dyn.clone(), false).await?;
        let on = open(&on_db, as_dyn.clone(), true).await?;
        wait_warm(&off).await?;
        wait_warm(&on).await?;
        let mut windows: Vec<Vec<Member>> = Vec::new();
        let (mut agree, mut compared, mut fired) = (0usize, 0usize, 0usize);
        let mut disagreements = Vec::new();
        for q in &queries {
            let base = recall(&off, q).await?;
            let r = recall(&on, q).await?;
            windows.push(
                base.hits
                    .into_iter()
                    .map(|h| Member {
                        id: h.node_id,
                        score: h.score,
                        content: h.content,
                        ty: h.concept_type.map(|t| format!("{t:?}")).unwrap_or_default(),
                    })
                    .collect(),
            );
            let w = windows.last().expect("just pushed");
            // Validation: the pipeline with the rule on at top_k 20
            // (window 20) against the offline replay of rule (a) on the
            // same window.
            let replay = rule_a(&store, w, WINDOW_K, Some(SHIPPED));
            let replayed: Vec<NodeId> = replay.picked.iter().map(|&i| w[i].id).collect();
            let piped: Vec<NodeId> = r.hits.iter().map(|h| h.node_id).collect();
            compared += 1;
            fired += usize::from(!replay.skipped.is_empty());
            // The pipeline may fill freed slots from past the window's 20.
            if piped.len() >= replayed.len() && piped[..replayed.len()] == replayed[..] {
                agree += 1;
            } else if disagreements.len() < 5 {
                disagreements.push(q.id.clone());
            }
        }
        off.close().await.map_err(|e| e.to_string())?;
        on.close().await.map_err(|e| e.to_string())?;
        eprintln!("{condition}: validation {agree}/{compared} agree ({fired} with a skip)");

        // The arms.
        let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
        let mut supp: BTreeMap<String, Suppression> = BTreeMap::new();
        let mut skips: BTreeMap<String, usize> = BTreeMap::new();
        for (q, w) in queries.iter().zip(&windows) {
            for k in KS {
                let base = current(w, k);
                for arm in &arms {
                    let sel = match arm.as_str() {
                        "current" => base.clone(),
                        "content" => rule_a(&store, w, k, None),
                        "fl" => facility_location(&store, w, k),
                        a if a.starts_with("a@") => {
                            rule_a(&store, w, k, Some(a[2..].parse().unwrap()))
                        }
                        m => mmr(&store, w, k, m[4..].parse().unwrap()),
                    };
                    // Skips by what they joined (rule (a) arms only).
                    for &(kept, skipped, signal) in &sel.skipped {
                        let (rk, rs) = (&store.role[&w[kept].id], &store.role[&w[skipped].id]);
                        let what = match (rk.carries(), rs.carries()) {
                            (Some(a), Some(b)) if a == b => "copy",
                            _ if rk.anchor() == rs.anchor() => "partner",
                            _ => "other",
                        };
                        *skips
                            .entry(format!("{}/k{k}/{arm}/{what}/{signal:?}", q.split))
                            .or_default() += 1;
                    }
                    for budget in BUDGETS {
                        let (shown, _) = render(w, &sel, budget);
                        let (base_shown, _) = render(w, &base, budget);
                        let mut covered: HashSet<usize> = HashSet::new();
                        // A memory is a corpus fact (its copies count once)
                        // or a partner (a distinct fact of its own).
                        let mut memories: HashSet<String> = HashSet::new();
                        let mut grade2 = false;
                        for &i in &shown {
                            let role = &store.role[&w[i].id];
                            if let Some(c) = role.carries() {
                                memories.insert(format!("corpus:{c}"));
                                covered.extend(covers(q, corpus[c]));
                                grade2 |= q.relevant.iter().any(|r| r.concept == c && r.grade == 2);
                            } else {
                                memories.insert(format!("node:{}", w[i].id.0));
                            }
                        }
                        let tokens_used = tokens(&render(w, &sel, budget).1);
                        for key in [
                            format!("{}/k{k}/b{budget}/{arm}", q.split),
                            format!("{}/k{k}/b{budget}/{arm}/family/{}", q.split, q.family),
                        ] {
                            let a = acc.entry(key).or_default();
                            a.n += 1;
                            a.facets += covered.len() as f64 / q.facets.len().max(1) as f64;
                            a.facet_hits += covered.len();
                            a.facet_total += q.facets.len();
                            a.distinct += memories.len();
                            a.rendered += shown.len();
                            a.tokens += tokens_used;
                            a.changed += usize::from(shown != base_shown);
                            a.first_relevant += usize::from(grade2);
                        }
                        // False suppression of a labelled partner pair.
                        let ids = |v: &[usize]| -> HashSet<NodeId> {
                            v.iter().map(|&i| w[i].id).collect()
                        };
                        let (now, before) = (ids(&shown), ids(&base_shown));
                        for &i in &base_shown {
                            let Role::Partner {
                                anchor,
                                family,
                                pair,
                            } = &store.role[&w[i].id]
                            else {
                                continue;
                            };
                            let Some(&a) = base_shown
                                .iter()
                                .find(|&&j| matches!(&store.role[&w[j].id], Role::Corpus(c) if c == anchor))
                            else {
                                continue;
                            };
                            let s = supp
                                .entry(format!("{}/k{k}/b{budget}/{arm}/{family}", q.split))
                                .or_default();
                            s.both_in_current += 1;
                            let (pi, ai) = (w[i].id, w[a].id);
                            if (now.contains(&pi) != now.contains(&ai))
                                && before.contains(&pi)
                                && before.contains(&ai)
                            {
                                s.suppressed += 1;
                                s.pairs.push(format!("{pair} in {}", q.id));
                            }
                        }
                    }
                }
            }
            lines.push(
                json!({
                    "condition": condition, "id": q.id, "split": q.split, "family": q.family,
                    "window": w.iter().map(|m| json!({
                        "role": format!("{:?}", store.role[&m.id]),
                        "score": r4(m.score),
                    })).collect::<Vec<_>>(),
                })
                .to_string(),
            );
        }
        report.insert(
            condition.clone(),
            json!({
                "concepts": store.role.len(),
                "semantic_edges": store.semantic.len(),
                "contradicts_edges": store.contradicts.len(),
                "validation": {"compared": compared, "agree": agree, "with_skip": fired,
                               "disagreements": disagreements},
                "metrics": acc.into_iter().map(|(k, a)| (k, a.json())).collect::<BTreeMap<_, _>>(),
                "suppression": supp.into_iter().map(|(k, s)| (k, json!({
                    "both_in_current": s.both_in_current, "suppressed": s.suppressed,
                    "pairs": s.pairs,
                }))).collect::<BTreeMap<_, _>>(),
                "skips": skips,
            }),
        );
    }
    let _ = std::fs::remove_dir_all(&work);
    let out = json!({
        "issue": 181,
        "embedder_kind": kind,
        "model": model,
        "window_k": WINDOW_K,
        "ks": KS,
        "budgets": BUDGETS,
        "shipped": format!("a@{SHIPPED:.2}"),
        "cache": {"hits": embedder.hits.load(Ordering::Relaxed), "misses": embedder.misses.load(Ordering::Relaxed)},
        "conditions": report,
    });
    let text = serde_json::to_string_pretty(&out).expect("json");
    match &args.out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, text + "\n").map_err(|e| e.to_string())?;
            std::fs::write(
                path.with_extension("windows.jsonl"),
                lines.join("\n") + "\n",
            )
            .map_err(|e| e.to_string())?;
        }
        None => println!("{text}"),
    }
    Ok(())
}

/// The shared window's recall: the session's own top 20, under a budget
/// nothing reaches.
async fn recall(mem: &Memory, q: &schema::Query) -> Result<lambo::RecallResult, String> {
    mem.recall(RecallQuery {
        query: q.query.clone(),
        top_k: WINDOW_K,
        max_tokens: 1_000_000,
        traversal_depth: mem.config().default_traversal_depth,
    })
    .await
    .map_err(|e| format!("recalling {}: {e}", q.id))
}

async fn wait_warm(mem: &Memory) -> Result<(), String> {
    let waited = Instant::now();
    while mem.stats().daemon_cycles == 0 {
        if waited.elapsed() > Duration::from_secs(60) {
            return Err("the daemon's warm-up cycle never ran".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
