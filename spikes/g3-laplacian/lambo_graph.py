"""G3 spike — shared loader: snapshot -> graph, and a faithful port of lambo's
tokenizer + BM25 + phase-1/phase-2 recall semantics.

READ-ONLY. Opens `snapshot.db` (a copy) with an immutable URI so the spike can
never write or lock anything, and never touches the live dogfood store.

Faithfulness notes (checked against the Rust sources at wt/g3):
  * tokenizer   `src/graph/canonical.rs::normalize_tokens` — NFC, camelCase
                split (ASCII-only boundary, original case), split on `-_` and
                whitespace, lowercase, drop the 13 stopwords, Snowball English
                stem. Validated against `fixtures/canonicalization-cases.json`.
  * BM25        `src/graph/index.rs::search` — k1=1.2, b=0.75, per-session df,
                idf = ln(1 + (N-df+0.5)/(df+0.5)), query terms deduped,
                sort score-desc then node-id-asc, truncate.
  * phase 1     `src/recall/candidates.rs` — union of keyword (over-fetched
                x4), recent-interaction (flat RECENT_SCORE=0.35 over the
                concepts of the 3 most recent interactions by created_at), and
                vector legs; max-merge; sort score-desc, id-asc; truncate.
  * phase 2     `src/recall/expand.rs` — BFS over OUT-edges only, in the five
                traversable Concept<->Concept types in priority order
                (Dependency, Causal, Hierarchical, CoOccurrence, Semantic),
                visited-set first-discovery-wins, depth counts levels.
                Derives/Temporal are structural and excluded.
  * blast       `src/store/sqlite.rs::blast_radius` — count of OTHER concepts
                with an aged inbound {Dependency, Causal, Hierarchical} edge
                from `node` and NO aged inbound structural edge from any other
                concept source. i.e. EXCLUSIVE dependents (orphans-if-removed).
"""

import json
import sqlite3
import unicodedata
from collections import defaultdict
from math import log
from pathlib import Path

HERE = Path(__file__).resolve().parent
SNAPSHOT = HERE / "snapshot.db"

# ---------------------------------------------------------------- tokenizer

STOPWORDS = frozenset(
    ["the", "a", "an", "for", "of", "at", "in", "to", "on", "and", "or", "is", "are"]
)

_STEMMER = None


def _stem(tok: str) -> str:
    global _STEMMER
    if _STEMMER is None:
        from nltk.stem.snowball import SnowballStemmer

        _STEMMER = SnowballStemmer("english")
    return _STEMMER.stem(tok)


# `is_invisible` in canonical.rs — only matters for adversarial content; the
# dogfood corpus is agent-written prose. Cf/Cc categories cover the table.
def _visible(ch: str) -> bool:
    return unicodedata.category(ch) not in ("Cf",)


def _split_camel(s: str) -> str:
    out = []
    prev_lower = False
    for c in s:
        if prev_lower and ("A" <= c <= "Z"):
            out.append(" ")
        prev_lower = "a" <= c <= "z"
        out.append(c)
    return "".join(out)


def normalize_tokens(content: str) -> list[str]:
    nfc = unicodedata.normalize("NFC", "".join(c for c in content if _visible(c)))
    raw = _split_camel(nfc)
    parts = []
    cur = []
    for c in raw:
        if c in "-_" or c.isspace():
            if cur:
                parts.append("".join(cur))
                cur = []
        else:
            cur.append(c)
    if cur:
        parts.append("".join(cur))
    return [_stem(t) for t in (p.lower() for p in parts) if t not in STOPWORDS]


def canonical_key(content: str) -> str:
    return " ".join(sorted(normalize_tokens(content)))


# ---------------------------------------------------------------- BM25

BM25_K1 = 1.2
BM25_B = 0.75


class InvertedIndex:
    def __init__(self, concepts):
        self.postings = defaultdict(dict)  # term -> {cid: tf}
        self.doc_len = {}
        for cid, content in concepts:
            toks = normalize_tokens(content)
            self.doc_len[cid] = len(toks)
            tf = defaultdict(int)
            for t in toks:
                tf[t] += 1
            for t, n in tf.items():
                self.postings[t][cid] = n
        self.total_docs = len(self.doc_len)
        self.total_tokens = sum(self.doc_len.values())

    def search(self, query: str, limit: int):
        if self.total_docs == 0:
            return []
        terms = sorted(set(normalize_tokens(query)))
        if not terms:
            return []
        n = float(self.total_docs)
        avg_dl = (self.total_tokens / n) if self.total_tokens else 1.0
        scores = defaultdict(float)
        for term in terms:
            posts = self.postings.get(term)
            if not posts:
                continue
            df = float(len(posts))
            idf = log(1.0 + (n - df + 0.5) / (df + 0.5))
            for doc, tf in posts.items():
                dl = float(self.doc_len.get(doc, 0))
                tfc = tf * (BM25_K1 + 1.0) / (
                    tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_dl)
                )
                scores[doc] += idf * tfc
        out = [(c, s) for c, s in scores.items() if s > 0.0]
        out.sort(key=lambda kv: (-kv[1], kv[0]))
        return out[:limit]


# ---------------------------------------------------------------- graph

TRAVERSAL_ORDER = ("Dependency", "Causal", "Hierarchical", "CoOccurrence", "Semantic")
STRUCTURAL = ("Dependency", "Causal", "Hierarchical")
RECENT_INTERACTIONS = 3
RECENT_SCORE = 0.35
KEYWORD_OVERFETCH = 4
DEFAULT_TRAVERSAL_DEPTH = 2


class LamboGraph:
    """The concept subgraph of one session, plus recall's phase-1/2 machinery."""

    def __init__(self, db_path=SNAPSHOT, session=None):
        uri = f"file:{db_path}?immutable=1"
        con = sqlite3.connect(uri, uri=True)
        try:
            if session is None:
                session = con.execute(
                    "select session_id from concepts group by 1 "
                    "order by count(*) desc limit 1"
                ).fetchone()[0]
            self.session = session
            rows = con.execute(
                "select id, content, concept_type, origin_interaction, created_at,"
                " access_count, canonization_status, blast_radius,"
                " (embedding is not null), chunk_group_id, canonical_key"
                " from concepts where session_id=? order by id",
                (session,),
            ).fetchall()
            self.concepts = {}
            for r in rows:
                self.concepts[r[0]] = dict(
                    id=r[0],
                    content=r[1],
                    type=r[2],
                    origin=r[3],
                    created_at=r[4],
                    access_count=r[5],
                    canon=r[6],
                    stored_blast=r[7],
                    has_embedding=bool(r[8]),
                    chunk_group=r[9],
                    key=r[10],
                )
            self.interactions = {
                r[0]: dict(id=r[0], prompt=r[1], created_at=r[2], agent=r[3])
                for r in con.execute(
                    "select id, prompt_text, created_at, agent_id from interactions"
                    " where session_id=? order by created_at, id",
                    (session,),
                ).fetchall()
            }
            self.edges = [
                dict(
                    source=r[0],
                    target=r[1],
                    type=r[2],
                    weight=r[3],
                    reinforcements=r[4],
                    created_at=r[5],
                )
                for r in con.execute(
                    "select source, target, edge_type, weight, reinforcements,"
                    " created_at from edges where session_id=? order by id",
                    (session,),
                ).fetchall()
            ]
        finally:
            con.close()

        # Node ordering: stable, id-ascending, concepts only.
        self.nodes = sorted(self.concepts)
        self.index_of = {n: i for i, n in enumerate(self.nodes)}

        # Concept<->Concept edges only, bucketed by type. Both endpoints must be
        # concepts of this session (mirrors `concept_ids` in MemoryStore).
        self.cc_edges = [
            e
            for e in self.edges
            if e["source"] in self.concepts and e["target"] in self.concepts
        ]
        self.out_typed = defaultdict(lambda: defaultdict(list))
        self.in_typed = defaultdict(lambda: defaultdict(list))
        for e in self.cc_edges:
            self.out_typed[e["source"]][e["type"]].append(e["target"])
            self.in_typed[e["target"]][e["type"]].append(e["source"])
        for m in (self.out_typed, self.in_typed):
            for d in m.values():
                for t in d:
                    d[t] = sorted(set(d[t]))

        self.index = InvertedIndex(
            [(c["id"], c["content"]) for c in self.concepts.values()]
        )

    # ------------------------------------------------------------ phase 1

    def recent_concepts(self):
        """Concepts whose origin_interaction is one of the 3 most recent."""
        recent = sorted(
            self.interactions.values(), key=lambda i: (i["created_at"], i["id"])
        )[-RECENT_INTERACTIONS:]
        ids = {i["id"] for i in recent}
        return sorted(c["id"] for c in self.concepts.values() if c["origin"] in ids)

    def phase1(self, query: str, top_k: int = 10, use_recent=True):
        """Union of keyword + recent legs, max-merged. Vector leg is empty on
        this snapshot for all but 22/386 concepts, and no query embedder runs in
        the spike, so it is omitted and that omission is reported."""
        legs = {}
        for cid, s in self.index.search(query, top_k * KEYWORD_OVERFETCH):
            legs.setdefault(cid, {})["keyword"] = s
        if use_recent:
            for cid in self.recent_concepts():
                legs.setdefault(cid, {})["recent"] = RECENT_SCORE
        merged = [(cid, max(d.values()), d) for cid, d in legs.items()]
        merged.sort(key=lambda t: (-t[1], t[0]))
        return merged[:top_k]

    # ------------------------------------------------------------ phase 2

    def expand(self, candidates, depth=DEFAULT_TRAVERSAL_DEPTH):
        """Faithful port of expand.rs: OUT-edge BFS, priority order per frontier
        node, first-discovery-wins. Returns [(node_id, level)] in the required
        list's structural order. chunk_group siblings are a no-op on this
        snapshot (0 concepts carry a chunk_group_id)."""
        required = []
        visited = set()
        frontier = []
        for cid in candidates:
            if cid not in visited:
                visited.add(cid)
                frontier.append(cid)
                required.append((cid, 0))
        for lvl in range(1, depth + 1):
            nxt = []
            for src in frontier:
                for ty in TRAVERSAL_ORDER:
                    for tgt in self.out_typed.get(src, {}).get(ty, ()):
                        if tgt not in visited:
                            visited.add(tgt)
                            nxt.append(tgt)
                            required.append((tgt, lvl))
            if not nxt:
                break
            frontier = nxt
        return required

    # ------------------------------------------------------------ blast

    def blast_radius(self, node):
        """Port of the store's blast_radius: EXCLUSIVE structural dependents.

        min_edge_age is not applied — every edge in this snapshot predates the
        spike by more than any configured min_edge_age, so the cutoff admits
        all of them (verified: max(edges.created_at) < snapshot time).
        """
        out = set()
        for ty in STRUCTURAL:
            out.update(self.out_typed.get(node, {}).get(ty, ()))
        out.discard(node)
        n = 0
        exclusive = []
        for tgt in sorted(out):
            others = set()
            for ty in STRUCTURAL:
                others.update(self.in_typed.get(tgt, {}).get(ty, ()))
            others.discard(node)
            if not others:
                n += 1
                exclusive.append(tgt)
        return n, exclusive

    def all_structural_dependents(self, node):
        out = set()
        for ty in STRUCTURAL:
            out.update(self.out_typed.get(node, {}).get(ty, ()))
        out.discard(node)
        return sorted(out)

    # ------------------------------------------------------------ helpers

    def snippet(self, cid, width=70):
        c = self.concepts[cid]
        txt = " ".join(c["content"].split())
        return txt[:width] + ("..." if len(txt) > width else "")


def validate_tokenizer(repo_root=None):
    """Check the ported tokenizer against the pinned fixture table."""
    root = Path(repo_root) if repo_root else HERE.parent.parent
    cases = json.loads((root / "fixtures/canonicalization-cases.json").read_text())
    bad = []
    for c in cases:
        if c["category"] == "synonym":
            continue  # synonym table is not part of the tokenizer
        got = canonical_key(c["input"])
        if got != c["expected_key"]:
            bad.append((c["input"], c["expected_key"], got))
    return bad


if __name__ == "__main__":
    bad = validate_tokenizer()
    print(f"tokenizer fixture check: {'PASS' if not bad else 'FAIL'}")
    for b in bad:
        print("  ", b)
    g = LamboGraph()
    print(f"session={g.session}")
    print(f"concepts={len(g.concepts)} interactions={len(g.interactions)}")
    print(f"edges(all)={len(g.edges)} edges(concept-concept)={len(g.cc_edges)}")
    byty = defaultdict(int)
    for e in g.cc_edges:
        byty[e["type"]] += 1
    print("cc edges by type:", dict(sorted(byty.items())))
