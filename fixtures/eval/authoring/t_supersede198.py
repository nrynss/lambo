"""#198: supersession pairs and the trigger's hard negatives.

#198's suspected-supersession trigger looks at a vector hit the #195
identifier guard refused, and keeps it only when both concepts have the same
type (Logic, Constraint or Observation), the same repo_scope, and
near-identical text once identifiers and digits are masked. These pairs give
it something to be measured on (precision >= 0.90 per embedder):

Every call names its pair's id (`"s01"` is p-198-s01), so an id never
moves when a pair is added, removed or reordered, and its split (`dev` or
`held_out`), so the split is not the id hash's: it is stratified by
sub-kind, about 60/40 within each of supersedes, distinct_entity and
same-event, each cut again by whether the two texts are identical once
identifiers and digits are masked (build.py checks the shares). Held-out
is where the trigger's precision is reported, so it gets its share of
the hardest negatives and at least 25 in-regime pairs of each kind.

  S()  supersedes: the two texts state the current value of one
       single-valued slot (a pin, a default, a constant, an unscoped current
       measurement, a count of this set) and only the value changed; the
       first text is the newer one (pairs.json's a).
  N()  distinct_entity hard negatives in the trigger's own regime: same
       type, same template, and both stay true, because each names its own
       scope (a version, a review round, an issue, an embedder, a size, a
       split). Where the facts allow, the two texts differ only in
       identifiers and digits (an embedder is named by its model id, a
       count in digits, both texts in one tense); where they cannot (a
       verdict word, a constant's name, dev against held_out, with against
       without fusion), the differing words stay, and such a pair is not
       counted as identifier-only.
  E()  same-event hard negatives: two records of one event whose digit runs
       differ (a date added, the order changed), so the guard refuses them;
       a paraphrase, never a replacement. A trigger hit on one is a false
       positive (owner decision, 2026-10-27).

Every pair says the type each text would be derived as (`types`, a then b).
Every text restates a fact from this repository's records, cited as `src`.
Every pair is refused by the identifier guard (build.py checks it).
All are authored: the dogfood store's own texts are not public (see the
README's hygiene section), so real store pairs reach this set only through
the owner's sheet (scripts/eval/fold_owner_labels.py).
"""
from common import P

L, C, O = "Logic", "Constraint", "Observation"
LABELLED_ON = "2026-10-27"
LABELLER = "claude-opus-5-5 (agent)"
_ids = set()


def _id(kind, key):
    """The pair's stable id, p-198-<key>, written at the call: an id never
    depends on where the call sits in this file."""
    assert key[0] == kind and key[1:].isdigit(), key
    pid = f"p-198-{key}"
    assert pid not in _ids, pid
    _ids.add(pid)
    return pid


def S(key, split, newer, older, t, src, review=None):
    P(newer, older, "related_distinct", "identifier", src, relation="supersedes",
      review=review, types=(t, t), id=_id("s", key), labelled_on=LABELLED_ON,
      labeller=LABELLER, split=split)


def N(key, split, a, b, t, src, review=None):
    P(a, b, "related_distinct", "identifier", src, relation="distinct_entity",
      review=review, types=(t, t), id=_id("n", key), labelled_on=LABELLED_ON,
      labeller=LABELLER, split=split)


def E(key, split, a, b, t, src):
    P(a, b, "paraphrase", "same-event", src, types=(t, t), id=_id("e", key),
      labelled_on=LABELLED_ON, labeller=LABELLER, split=split)


CL = "CHANGELOG.md"
DF = "dev-diary/lambo-for-mooshik/DOGFOOD.md"
AR = "dev-diary/adversarial-review/"
EV = "fixtures/eval/README.md"

# ---- supersedes: pins ----------------------------------------------------
S("s01", "dev", "The Mac dogfood rig serves the pinned binary lambo-21e4cf8, built with store-sqlite,embed-candle-metal,embed-bge.",
  "The Mac dogfood rig serves the pinned binary lambo-19f51d3, built with store-sqlite,embed-candle-metal,embed-bge.",
  L, f"{DF} (open question 1: pin 21e4cf8; superseded pins 3039b82, then 19f51d3)")
S("s02", "held_out", "Current Mac rig pin: 19f51d3, the binary every client's MCP registration spawns.",
  "Current Mac rig pin: 3039b82, the binary every client's MCP registration spawns.",
  L, f"{DF} (open question 1: superseded pins 3039b82, then 19f51d3)")
S("s03", "dev", "The Linux dogfood rig (CUDA build) is pinned to bbef4b3.",
  "The Linux dogfood rig (CUDA build) is pinned to fe4d63d.",
  L, f"{DF} (Linux rig, 2026-08-24: re-pinned fe4d63d to bbef4b3)")
S("s04", "held_out", "The dogfood writer and portal run the pinned lambo 0.6.0 binary.",
  "The dogfood writer and portal run the pinned lambo 0.5.0 binary.",
  L, "evidence/0.6.0-vs-0.5.0/README.md (Builds; Re-pin)")
S("s05", "dev", "The dogfood writer runs the pinned lambo 0.5.0 release.",
  "The dogfood writer runs the pinned lambo 0.4.0 release.",
  L, "evidence/0.5.0-vs-0.4.0/README.md (the 0.5.0 re-pin); evidence/0.6.0-vs-0.5.0/README.md (0.5.0 is the rig's pinned binary)")

# ---- supersedes: toolchain, constants, defaults --------------------------
S("s06", "dev", "The pinned Rust toolchain is 1.99.0; CI and the release workflow install it directly.",
  "The pinned Rust toolchain is 1.97.1; CI and the release workflow install it directly.",
  C, f"{CL} (0.4.0: the pinned toolchain moves from 1.97.1 to 1.99.0, #42)")
S("s07", "held_out", "The lambo crate is built on Rust edition 2024.",
  "The lambo crate is built on Rust edition 2021.",
  C, f"{CL} (0.4.0: the crate moves from edition 2021 to edition 2024, #42)")
S("s08", "dev", "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 18.",
  "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 17.",
  C, "src/store/batch.rs (CONCEPT_COLUMNS = 18); CHANGELOG.md 0.3.0 (16 to 17)")
S("s09", "held_out", "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 17.",
  "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 16.",
  C, f"{CL} (0.3.0: CONCEPT_COLUMNS 16 to 17)")
S("s10", "dev", "lambo::store::batch::INTERACTION_COLUMNS, the bind-parameter count of one interaction row, is 7.",
  "lambo::store::batch::INTERACTION_COLUMNS, the bind-parameter count of one interaction row, is 6.",
  C, f"{CL} (0.3.0: INTERACTION_COLUMNS 6 to 7)")
S("s11", "dev", "lambo::store::batch::EDGE_COLUMNS, the bind-parameter count of one edge row, is 10.",
  "lambo::store::batch::EDGE_COLUMNS, the bind-parameter count of one edge row, is 9.",
  C, f"{CL} (0.3.0: EDGE_COLUMNS 9 to 10)")
S("s12", "held_out", "The longest lambo_stats wait_ms a caller may ask for is 34000.",
  "The longest lambo_stats wait_ms a caller may ask for is 4000.",
  C, f"{CL} (0.4.0: the longest wait_ms is now 34000, was 4000, #11)")
S("s13", "dev", "Under the EmbeddingGemma 2 contract, the recent-interactions recall leg scores 0.63.",
  "Under the EmbeddingGemma 2 contract, the recent-interactions recall leg scores 0.35.",
  L, f"{CL} (Unreleased, #87: the recent leg is calibrated per contract)")
S("s14", "held_out", "A writer attaching under the EmbeddingGemma 2 contract merges at a semantic match threshold of 0.93.",
  "A writer attaching under the EmbeddingGemma 2 contract merges at a semantic match threshold of 0.85.",
  L, f"{CL} (Unreleased, #87: EG2 writers merge at 0.93, not 0.85)")
S("s15", "dev", "Under EmbeddingGemma 2, a layer whose min_similarity is unset takes a floor of 0.72.",
  "Under EmbeddingGemma 2, a layer whose min_similarity is unset takes a floor of 0.50.",
  L, f"{CL} (Unreleased, #87: an unset EG2 layer floor is 0.72, not 0.50)")
S("s16", "dev", "embed-candle-cuda builds against CUDA 13.4 directly, with no version override.",
  "embed-candle-cuda needs CUDARC_CUDA_VERSION=13030 set to build against CUDA 13.4.",
  C, f"{CL} (0.5.0: CUDA 13.4 builds directly, #111)")
S("s17", "dev", "The latest published lambo release is 0.7.0 (2026-10-19).",
  "The latest published lambo release is 0.6.0 (2026-10-13).",
  O, f"{CL} (release headings 0.6.0 and 0.7.0)")

# ---- supersedes: unscoped current measurements ---------------------------
S("s18", "held_out", "Keyword recall over MCP on a 9,500-concept SQLite store runs at 7.5 ms p50.",
  "Keyword recall over MCP on a 9,500-concept SQLite store runs at 18.7 ms p50.",
  O, f"{CL} (0.7.0, #171); evidence/issue-171-perf/")
S("s19", "dev", "With 16 concurrent clients, a 9,500-concept SQLite store serves 1,104 recalls per second.",
  "With 16 concurrent clients, a 9,500-concept SQLite store serves 63 recalls per second.",
  O, f"{CL} (0.7.0, #171); evidence/issue-171-perf/")
S("s20", "held_out", "A back-to-back lambo_derive on a 9,500-concept SQLite store is acknowledged in 0.8 ms p50.",
  "A back-to-back lambo_derive on a 9,500-concept SQLite store is acknowledged in 24.8 ms p50.",
  O, f"{CL} (0.7.0, #171)")
S("s21", "dev", "A back-to-back lambo_derive on a 9,500-concept SQLite store is applied in 6.3 ms p50.",
  "A back-to-back lambo_derive on a 9,500-concept SQLite store is applied in 32.4 ms p50.",
  O, f"{CL} (0.7.0, #171)")
S("s22", "dev", "A clean lambo provision of a 9,508-concept store takes 5.6 ms p50.",
  "A clean lambo provision of a 9,508-concept store takes 143 ms p50.",
  O, f"{CL} (0.7.0, #171: provision skips the #148 re-key scan once it has run clean)")
S("s23", "held_out", "One daemon rescore of a 9,100-concept session takes 31 ms.",
  "One daemon rescore of a 9,100-concept session takes 201 ms.",
  O, f"{CL} (0.7.0, #171: a rescore walks incident edges once)")
S("s24", "dev", "A structural recall on a synthetic 9,100-concept store takes about 1 ms per query (release build).",
  "A structural recall on a synthetic 9,100-concept store takes about 70 ms per query (release build).",
  O, f"{CL} (0.7.0, #165)")
S("s25", "held_out", "A derive commit holds the session write lock for 16.5 µs at 10,000 concepts (release build).",
  "A derive commit holds the session write lock for 6.8 ms at 10,000 concepts (release build).",
  O, f"{CL} (0.5.0, #102)")
S("s26", "dev", "A derive commit holds the session write lock for 21 µs at 100,000 concepts (release build).",
  "A derive commit holds the session write lock for 168 ms at 100,000 concepts (release build).",
  O, f"{CL} (0.5.0, #102)")
S("s27", "held_out", "On Metal, the embedder of a lambo serve that becomes a proxy takes about 4 MB.",
  "On Metal, the embedder of a lambo serve that becomes a proxy takes about 1.4 GB.",
  O, f"{CL} (0.5.0, #31)")
S("s28", "held_out", "Warm recall p50 on a 3,600-concept SQLite session is 4.3 ms.",
  "Warm recall p50 on a 3,600-concept SQLite session is 219 ms.",
  O, f"{CL} (0.4.0, #8, #14: the session holder ranks vectors from memory)")
# s29 and s30 are distinct_entity by the owner's ruling of 2026-10-27 (each
# text names its own storage format, text or packed f32, and a store not yet
# provisioned on 0.7.0 still shows the text value), folded in from
# owner_pairs.json; their ids keep the s.
S("s29", "dev", "A 3,600-concept SQLite store at dim 1024 takes 17.1 MB on disk.",
  "A 3,600-concept SQLite store at dim 1024 takes 47.3 MB on disk.",
  O, f"{CL} (0.7.0, #55: packed f32 vectors)")
S("s30", "held_out", "Decoding every vector of a 3,600-concept SQLite store at dim 1024 takes 3.4 ms.",
  "Decoding every vector of a 3,600-concept SQLite store at dim 1024 takes 161 ms.",
  O, f"{CL} (0.7.0, #55)")
S("s31", "dev", "On a copy of the Metal rig store, GC's first sweep collects 0 concepts.",
  "On a copy of the Metal rig store, GC's first sweep collects 537 concepts.",
  O, f"{CL} (0.3.0, GC: the first sweep now collects nothing, was 537)")
S("s32", "dev", "GC's first sweep on the Metal rig copy holds the write lock for 9.0 ms.",
  "GC's first sweep on the Metal rig copy holds the write lock for 67.5 ms.",
  O, f"{CL} (0.3.0, GC: sweeps are cheap)")
S("s33", "held_out", "In the REST API fixture, user schema has 14 dependents.",
  "In the REST API fixture, user schema has 8 dependents.",
  O, f"{CL} (0.4.0: user schema's eight fields are parent_of children, 14 dependents, was 8)")
S("s34", "dev", "The demo's user schema reports 15 dependents.",
  "The demo's user schema reports 9 dependents.",
  O, f"{CL} (0.4.0: the demo's user schema reports 15, was 9)")

# ---- supersedes: counts of this set (git history of the README) -----------
S("s35", "held_out", "The #190 evaluation set holds 368 queries.",
  "The #190 evaluation set holds 310 queries.",
  O, f"{EV} (git 3d421739: 310 to 368 queries)")
S("s36", "dev", "The #190 set has 262 answerable queries.",
  "The #190 set has 204 answerable queries.",
  O, f"{EV} (git 3d421739: 204 to 262 answerable)")
S("s37", "dev", "The #190 queries split 239 dev / 129 held-out.",
  "The #190 queries split 199 dev / 111 held-out.",
  O, f"{EV} (git 3d421739)")
S("s38", "held_out", "The #190 set has 48 vocabulary-mismatch queries.",
  "The #190 set has 12 vocabulary-mismatch queries.",
  O, f"{EV} (git 3d421739: 12 to 48)")
S("s39", "dev", "The #190 set has 17 rationale queries.",
  "The #190 set has 8 rationale queries.",
  O, f"{EV} (git 3d421739: 8 to 17)")
S("s40", "held_out", "The #190 set has 36 contrast queries.",
  "The #190 set has 23 contrast queries.",
  O, f"{EV} (git 3d421739: 23 to 36)")
S("s41", "dev", "Of the #190 related-distinct pairs, 82 carry the relation contradicts.",
  "Of the #190 related-distinct pairs, 84 carry the relation contradicts.",
  O, f"{EV} (git 8d2269a9: the owner's rulings, 84 to 82)")

# ---- supersedes: added for the review (2026-10-27) ------------------------
S("s42", "dev", "Under BAAI/bge-m3, a layer with no min_similarity set takes a floor of 0.50.",
  "Under BAAI/bge-m3, a layer with no min_similarity set takes a floor of 0.40.",
  L, f"{CL} (0.6.0, Upgrading from 0.5.0, #157: the default min_similarity rises from 0.40 to 0.50)")
S("s43", "held_out", "lambo::recall::candidates::RECENT_SCORE, the recent-interactions leg's flat score, is 0.35.",
  "lambo::recall::candidates::RECENT_SCORE, the recent-interactions leg's flat score, is 0.5.",
  C, f"{CL} (0.3.0: RECENT_SCORE dropped from 0.5 to 0.35); src/recall/candidates.rs")
S("s44", "dev", "A derive commit holds the session write lock for 17 µs at 50,000 concepts (release build).",
  "A derive commit holds the session write lock for 36.2 ms at 50,000 concepts (release build).",
  O, f"{CL} (0.5.0, #102)")
S("s45", "dev", "On a 3,600-concept session, a one-concept derive goes from acknowledged to applied in 14 ms (fixture embedder, release build).",
  "On a 3,600-concept session, a one-concept derive goes from acknowledged to applied in 246 ms (fixture embedder, release build).",
  O, f"{CL} (0.4.0, #8)")
S("s46", "held_out", "On a 3,600-concept session, a three-concept derive goes from acknowledged to applied in 20 ms (fixture embedder, release build).",
  "On a 3,600-concept session, a three-concept derive goes from acknowledged to applied in 670 ms (fixture embedder, release build).",
  O, f"{CL} (0.4.0, #8)")
S("s47", "dev", "With candle BGE-M3 on Metal, a repeated warm recall on a 3,600-concept SQLite session takes 4.1 ms p50.",
  "With candle BGE-M3 on Metal, a repeated warm recall on a 3,600-concept SQLite session takes 22.3 ms p50.",
  O, f"{CL} (0.4.0, #14: the session holder keeps recent query vectors)")
S("s48", "dev", "With candle BGE-M3 on Metal, a repeated recall with a derive applied in between takes 3.6 ms p50 on a 3,600-concept SQLite session.",
  "With candle BGE-M3 on Metal, a repeated recall with a derive applied in between takes 21.5 ms p50 on a 3,600-concept SQLite session.",
  O, f"{CL} (0.4.0, #14)")
S("s49", "held_out", "The Mac dogfood rig serves the pinned binary lambo-4c6fc93, built with store-sqlite,embed-candle-metal,embed-bge.",
  "The Mac dogfood rig serves the pinned binary lambo-21e4cf8, built with store-sqlite,embed-candle-metal,embed-bge.",
  L, "corpus emb-06 and its contrast (dogfood texts: launchd repointed from lambo-21e4cf8 to lambo-4c6fc93)")
S("s50", "dev", "The Mac dogfood rig serves the pinned binary lambo-e11fb06, built with store-sqlite,embed-candle-metal,embed-bge.",
  "The Mac dogfood rig serves the pinned binary lambo-4c6fc93, built with store-sqlite,embed-candle-metal,embed-bge.",
  L, "corpus emb-06 (dogfood text: re-pinned to lambo-e11fb06, repointed from lambo-4c6fc93)")
S("s51", "held_out", "Identifier recall over MCP on a 9,508-concept SQLite store runs at 6.7 ms p50.",
  "Identifier recall over MCP on a 9,508-concept SQLite store runs at 14.8 ms p50.",
  O, "evidence/issue-171-perf/README.md (PR #183 table: identifier_recall p50 14.82 to 6.71 ms)")
S("s52", "dev", "A structural recall that misses its anchor on a 9,508-concept SQLite store runs at 9.3 ms p50.",
  "A structural recall that misses its anchor on a 9,508-concept SQLite store runs at 20.5 ms p50.",
  O, "evidence/issue-171-perf/README.md (PR #183 table: structural_miss p50 20.45 to 9.33 ms)")
S("s53", "dev", "A structural recall that hits its anchor on a 9,508-concept SQLite store runs at 1.85 ms p50.",
  "A structural recall that hits its anchor on a 9,508-concept SQLite store runs at 2.35 ms p50.",
  O, "evidence/issue-171-perf/README.md (PR #183 table: structural_hit p50 2.35 to 1.85 ms)")
S("s54", "held_out", "Keyword recall over MCP on a 9,508-concept SQLite store runs at 9.7 ms p90.",
  "Keyword recall over MCP on a 9,508-concept SQLite store runs at 23.2 ms p90.",
  O, "evidence/issue-171-perf/README.md (PR #183 table: keyword_semantic_recall p90 23.2 to 9.68 ms)")
S("s55", "dev", "With 16 concurrent clients on a 9,508-concept SQLite store, one recall takes 13.1 ms p50.",
  "With 16 concurrent clients on a 9,508-concept SQLite store, one recall takes 249.8 ms p50.",
  O, "evidence/issue-171-perf/README.md (PR #183 table: recall_16_concurrent p50 249.8 to 13.06 ms)")
S("s56", "held_out", "One daemon rescore of a 5,000-concept session takes 8.0 ms.",
  "One daemon rescore of a 5,000-concept session takes 62.4 ms.",
  O, "evidence/issue-171-perf/README.md (rescore table: 5,000 concepts 62.4 to 8.0 ms)")
S("s57", "dev", "Ranking 9,000 vector candidates at dim 1024 for a top 10 takes 1.85 ms per scan.",
  "Ranking 9,000 vector candidates at dim 1024 for a top 10 takes 8.93 ms per scan.",
  O, "evidence/issue-171-perf/README.md (micro-measurements: scalar cosine and full sort 8.93 ms, lane kernel and partial top-k 1.85 ms)")
S("s58", "dev", "A clean lambo provision of a 9,508-concept store takes 7.6 ms p90.",
  "A clean lambo provision of a 9,508-concept store takes 146.3 ms p90.",
  O, "evidence/issue-171-perf/README.md (PR #186 table: provision_ms p90 146.33 to 7.58)")
S("s59", "held_out", "A back-to-back lambo_derive on a 9,508-concept SQLite store is acknowledged in 14.2 ms p90.",
  "A back-to-back lambo_derive on a 9,508-concept SQLite store is acknowledged in 27.8 ms p90.",
  O, "evidence/issue-171-perf/README.md (PR #185 table: derive_ack_back_to_back p90 27.84 to 14.21 ms)")
S("s60", "dev", "On the Metal dogfood rig (4,949 concepts), lambo_recall with top_k 8 runs at 5.7 ms p50.",
  "On the Metal dogfood rig (4,949 concepts), lambo_recall with top_k 8 runs at 310.0 ms p50.",
  O, "evidence/issue-108-perf/README.md (Metal rig: 0.3.0 pin 310.0 ms, 0.4.0 pin 5.7 ms)")
S("s61", "held_out", "On the Metal dogfood rig (4,949 concepts), lambo_recall with top_k 8 runs at 12.2 ms p90.",
  "On the Metal dogfood rig (4,949 concepts), lambo_recall with top_k 8 runs at 332.7 ms p90.",
  O, "evidence/issue-108-perf/README.md (Metal rig p90: 332.7 to 12.2 ms)")
S("s62", "dev", "On the Metal dogfood rig (4,949 concepts), a lambo_derive goes from ack to applied in 35.9 ms p50.",
  "On the Metal dogfood rig (4,949 concepts), a lambo_derive goes from ack to applied in 317.8 ms p50.",
  O, "evidence/issue-108-perf/README.md (Metal rig: 317.8 to 35.9 ms)")
S("s63", "dev", "On the CUDA dogfood rig (8,496 concepts), lambo_recall with top_k 8 runs at 15.4 ms p50.",
  "On the CUDA dogfood rig (8,496 concepts), lambo_recall with top_k 8 runs at 804.8 ms p50.",
  O, "evidence/issue-108-perf/README.md (CUDA rig, #108: bef53e6 804.8 ms, 0.4.0 15.4 ms)")
S("s64", "held_out", "On the CUDA dogfood rig (8,496 concepts), a lambo_derive goes from ack to applied in 165.3 ms p50.",
  "On the CUDA dogfood rig (8,496 concepts), a lambo_derive goes from ack to applied in 2,933.5 ms p50.",
  O, "evidence/issue-108-perf/README.md (CUDA rig, #108: 2,933.5 to 165.3 ms)")

# ---- distinct_entity: review rounds (dev-diary/adversarial-review) --------
N("n01", "dev", "J1 round-1 adversarial review verdict: REQUEST_CHANGES with one P1.",
  "J2 round-1 adversarial review verdict: REQUEST_CHANGES with one P1.",
  O, f"{AR}adve-review-mooshik-J1-round1.md, adve-review-mooshik-J2-round1.md")
N("n02", "held_out", "J3 round-1 adversarial review verdict: REQUEST_CHANGES, 1 P1, 4 P2, 8 P3.",
  "J3 round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  O, f"{AR}adve-review-mooshik-J3-round1.md, adve-review-mooshik-J3-round2.md")
N("n03", "dev", "J3 round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  "J3 redesign round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 5 P3.",
  O, f"{AR}adve-review-mooshik-J3-round2.md, adve-review-mooshik-J3-redesign-round2.md")
N("n04", "dev", "J3 round-3 adversarial review verdict: REQUEST_CHANGES with 2 new P1, 1 P2 and 3 P3.",
  "J3 round-2 adversarial review verdict: REQUEST_CHANGES with 1 new P1, 3 P2 and 6 P3.",
  O, f"{AR}adve-review-mooshik-J3-round2.md, adve-review-mooshik-J3-round3.md")
N("n05", "held_out", "J1 round-2 adversarial review verdict: REQUEST_CHANGES, 0 P1, 2 P2 and 2 P3.",
  "J1 round-3 adversarial review verdict: CLEAN, 0 P1, 0 P2 and 3 P3.",
  O, f"{AR}adve-review-mooshik-J1-round2.md, adve-review-mooshik-J1-round3.md")
N("n06", "held_out", "J0 round-1 adversarial review verdict: CLEAN, 0 P2 and 6 P3 advisory findings, none blocking.",
  "J0 round-2 adversarial review verdict: CLEAN, 1 P2 and 5 P3 advisory findings, none blocking.",
  O, f"{AR}adve-review-mooshik-J0-round1.md, adve-review-mooshik-J0-round2.md")
N("n07", "dev", "J3 redesign round-1 review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  "J3 redesign round-2 review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 5 P3.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round1.md, adve-review-mooshik-J3-redesign-round2.md")
N("n08", "dev", "J end-to-end round-1 review verdict: REQUEST_CHANGES, 0 P1, 5 P2, 7 P3.",
  "J end-to-end round-2 review verdict: REQUEST_CHANGES, 0 P1, 0 P2, 7 P3.",
  O, f"{AR}adve-review-mooshik-J-e2e-round1.md, adve-review-mooshik-J-e2e-round2.md")
N("n09", "held_out", "J2 round-3 adversarial review verdict: CLEAN.",
  "J1 round-3 adversarial review verdict: CLEAN.",
  O, f"{AR}adve-review-mooshik-J1-round3.md, adve-review-mooshik-J2-round3.md")
N("n10", "dev", "J3 redesign round-3 review verdict: APPROVE.",
  "J3 redesign round-2 review verdict: REQUEST_CHANGES.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round2.md, adve-review-mooshik-J3-redesign-round3.md")
N("n11", "dev", "Review notes for J2 round 1 are in adve-review-mooshik-J2-round1.md.",
  "Review notes for J2 round 2 are in adve-review-mooshik-J2-round2.md.",
  O, f"{AR} (one file per round; #178's real pairs i3-07, i3-17)")
N("n12", "held_out", "Review notes for J3 round 1 are in adve-review-mooshik-J3-round1.md.",
  "Review notes for J3 round 3 are in adve-review-mooshik-J3-round3.md.",
  O, f"{AR} (one file per round; #178's real pair i3-05)")

# ---- distinct_entity: version-scoped measurements -------------------------
N("n13", "dev", "On 0.7.0, keyword recall over MCP on a 9,500-concept SQLite store runs at 7.5 ms p50.",
  "On 0.6.0, keyword recall over MCP on a 9,500-concept SQLite store runs at 18.7 ms p50.",
  O, f"{CL} (0.7.0, #171)")
N("n14", "dev", "On 0.7.0, 16 concurrent clients get 1,104 recalls per second from a 9,500-concept store.",
  "On 0.6.0, 16 concurrent clients get 63 recalls per second from a 9,500-concept store.",
  O, f"{CL} (0.7.0, #171)")
N("n15", "held_out", "0.7.0 acknowledges a back-to-back lambo_derive in 0.8 ms p50.",
  "0.6.0 acknowledges a back-to-back lambo_derive in 24.8 ms p50.",
  O, f"{CL} (0.7.0, #171)")
N("n16", "dev", "On 0.7.0 a structural recall on a 9,100-concept store takes about 1 ms.",
  "On 0.6.0 a structural recall on a 9,100-concept store takes about 70 ms.",
  O, f"{CL} (0.7.0, #165)")
N("n17", "held_out", "At 10,000 concepts a derive commit holds the write lock for 16.5 µs (0.5.0).",
  "At 50,000 concepts a derive commit holds the write lock for 17 µs (0.5.0).",
  O, f"{CL} (0.5.0, #102)")
N("n18", "dev", "At 50,000 concepts a derive commit holds the write lock for 17 µs.",
  "At 100,000 concepts a derive commit holds the write lock for 21 µs.",
  O, f"{CL} (0.5.0, #102)")
N("n19", "held_out", "With RRF fusion, whole-pipeline nDCG@10 on the #190 dev split under BGE-M3 is 0.893.",
  "Without RRF fusion, whole-pipeline nDCG@10 on the #190 dev split under BGE-M3 is 0.848.",
  O, f"{CL} (Unreleased, #173)")
N("n20", "dev", "With RRF fusion, nDCG@10 on the #190 dev split is 0.893 under BAAI/bge-m3.",
  "With RRF fusion, nDCG@10 on the #190 dev split is 0.889 under google/embeddinggemma-2.",
  O, f"{CL} (Unreleased, #173)")
N("n21", "dev", "With RRF fusion, nDCG@10 under BGE-M3 is 0.893 on the #190 dev split.",
  "With RRF fusion, nDCG@10 under BGE-M3 is 0.887 on the #190 held_out split.",
  O, f"{CL} (Unreleased, #173)")
N("n22", "dev", "Without fusion, nDCG@10 under BGE-M3 is 0.848 on the #190 dev split.",
  "Without fusion, nDCG@10 under BGE-M3 is 0.865 on the #190 held_out split.",
  O, f"{CL} (Unreleased, #173)")

# ---- distinct_entity: per-embedder calibrations (#87) ---------------------
N("n23", "held_out", "Under google/embeddinggemma-2 the recent-interactions leg scores 0.63.",
  "Under BAAI/bge-m3 the recent-interactions leg scores 0.35.",
  L, f"{CL} (Unreleased, #87)")
N("n24", "dev", "Under google/embeddinggemma-2 a writer merges at a threshold of 0.93.",
  "Under BAAI/bge-m3 a writer merges at a threshold of 0.85.",
  L, f"{CL} (Unreleased, #87)")
N("n25", "held_out", "Under google/embeddinggemma-2 an unset layer floor is 0.72.",
  "Under BAAI/bge-m3 an unset layer floor is 0.50.",
  L, f"{CL} (Unreleased, #87)")
N("n26", "dev", "At a 0.50 layer floor, 99.9% of irrelevant concepts on the #190 held-out split pass under google/embeddinggemma-2.",
  "At a 0.50 layer floor, 3.2% of irrelevant concepts on the #190 held-out split pass under BAAI/bge-m3.",
  O, f"{CL} (Unreleased, #87); evidence/issue-87-calibration/")
N("n27", "dev", "At the 0.85 threshold, about 78% of the #190 related-but-distinct pairs are over it by cosine under google/embeddinggemma-2.",
  "At the 0.85 threshold, about 35-44% of the #190 related-but-distinct pairs are over it by cosine under BAAI/bge-m3.",
  O, f"{CL} (Unreleased, #87)")

# ---- distinct_entity: constants and caps that share a template ------------
N("n28", "held_out", "lambo::store::batch::CONCEPT_COLUMNS is 18.",
  "lambo::store::batch::EDGE_COLUMNS is 10.",
  C, "src/store/batch.rs")
N("n29", "dev", "lambo::store::batch::INTERACTION_COLUMNS is 7 (src/store/batch.rs).",
  "lambo::store::batch::EDGE_COLUMNS is 10 (src/store/batch.rs).",
  C, "src/store/batch.rs")
N("n30", "held_out", "At most 16 lambo_stats waits run at once per server.",
  "At most 8 lambo_stats waits run at once per agent_id.",
  C, f"{CL} (0.4.0, #11: wait caps)")
N("n31", "dev", "lambo::graph::hybrid::MAX_HYBRID_CONCEPTS is 256 (src/graph/hybrid.rs).",
  "lambo::graph::hybrid::MAX_HYBRID_WORK_ITEMS is 512 (src/graph/hybrid.rs).",
  C, "src/graph/hybrid.rs")

# ---- distinct_entity: rigs, releases, merges, milestones ------------------
N("n32", "dev", "The Mac dogfood rig is pinned to 21e4cf8 (candle on Metal).",
  "The Linux dogfood rig is pinned to bbef4b3 (candle on CUDA).",
  L, f"{DF} (open question 1, both rigs as of 2026-08-24)")
N("n33", "held_out", "lambo 0.7.0 was released on 2026-10-19.",
  "lambo 0.6.0 was released on 2026-10-13.",
  O, f"{CL} (release headings)")
N("n34", "dev", "lambo 0.5.0 was released on 2026-10-12.",
  "lambo 0.4.0 was released on 2026-10-11.",
  O, f"{CL} (release headings)")
N("n35", "held_out", "Release 0.3.0 of lambo is dated 2026-10-07 in CHANGELOG.md.",
  "Release 0.4.0 of lambo is dated 2026-10-11 in CHANGELOG.md.",
  O, f"{CL} (release headings)")
N("n36", "dev", "PR #205 merged to main as c3ba6722 on 2026-10-27.",
  "PR #206 merged to main as 1ac09bf8 on 2026-10-27.",
  O, "git log --merges (c3ba6722, 1ac09bf8)")
N("n37", "dev", "PR #203 merged to main as 266f8bea on 2026-10-26.",
  "PR #204 merged to main as 8c92a5e9 on 2026-10-26.",
  O, "git log --merges (266f8bea, 8c92a5e9)")
N("n38", "held_out", "PR #199 merged to main as b8177774 on 2026-10-20.",
  "PR #200 merged to main as 437d3d58 on 2026-10-20.",
  O, "git log --merges (b8177774, 437d3d58)")
N("n39", "dev", "PR #195 merged to main as 5e44c692 on 2026-10-20.",
  "PR #194 merged to main as 3a76def7 on 2026-10-20.",
  O, "git log --merges (5e44c692, 3a76def7)")
N("n40", "held_out", "Issue #198 is planned for milestone 0.8.0.",
  "Issue #181 is planned for milestone 0.8.0.",
  L, "nrynss/lambo issues #181, #198 (milestone 0.8.0)")
N("n41", "dev", "Issue #207 is planned for milestone 0.8.0.",
  "Issue #208 is planned for milestone 0.8.0.",
  L, "nrynss/lambo issues #207, #208 (milestone 0.8.0)")
N("n42", "dev", "Issue #117 belongs to milestone 0.5.0.",
  "Issue #118 belongs to milestone 0.5.0.",
  L, "nrynss/lambo issues #117, #118 (milestone 0.5.0); CHANGELOG.md 0.5.0")
N("n43", "held_out", "Issue #180 belongs to milestone 0.8.0.",
  "Issue #116 belongs to milestone 0.5.0.",
  L, "nrynss/lambo issues #116 (0.5.0), #180 (0.8.0)")
N("n44", "dev", "Issue #176 shipped in lambo 0.7.0.",
  "Issue #178 shipped in lambo 0.7.0.",
  O, f"{CL} (0.7.0 highlights)")
N("n45", "held_out", "Issue #55 shipped in lambo 0.7.0.",
  "Issue #165 shipped in lambo 0.7.0.",
  O, f"{CL} (0.7.0 performance)")
N("n46", "dev", "Issue #102 shipped in lambo 0.5.0.",
  "Issue #31 shipped in lambo 0.5.0.",
  O, f"{CL} (0.5.0)")
N("n47", "held_out", "The dogfood writer listens on 127.0.0.1:7700.",
  "The old llama-server embedder listened on 127.0.0.1:8080.",
  O, "dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md (MCP url on 7700); DOGFOOD.md (bge_m3 at 127.0.0.1:8080)")
N("n48", "dev", "Phase 3 of the build is written up in dev-diary/PHASE-3-stores.md.",
  "Phase 4 of the build is written up in dev-diary/PHASE-4-daemon.md.",
  O, "dev-diary/PHASE-3-stores.md, dev-diary/PHASE-4-daemon.md")
N("n49", "held_out", "Phase 5 of the build is written up in dev-diary/PHASE-5-recall.md.",
  "Phase 6 of the build is written up in dev-diary/PHASE-6-canonization.md.",
  O, "dev-diary/PHASE-5-recall.md, dev-diary/PHASE-6-canonization.md")
N("n50", "dev", "PR #188 merged to main as d327bf77 on 2026-10-19.",
  "PR #187 merged to main as d38c81ee on 2026-10-19.",
  O, "git log --merges (d327bf77, d38c81ee)")
N("n51", "held_out", "Issue #209 belongs to milestone 0.8.0.",
  "Issue #181 belongs to milestone 0.8.0.",
  L, "nrynss/lambo issues #181, #209 (milestone 0.8.0)")
N("n52", "dev", "lambo::graph::hybrid::MAX_HYBRID_PARENT_PAIRS is 256.",
  "lambo::graph::hybrid::MAX_HYBRID_WORK_ITEMS is 512.",
  C, "src/graph/hybrid.rs")
N("n53", "held_out", "lambo::surface::limits::MAX_CONTENT_BYTES is 16,384 bytes.",
  "lambo::graph::hybrid::MAX_HYBRID_CONTEXT_BYTES is 16 * 1024 bytes.",
  C, "src/surface/limits.rs; src/graph/hybrid.rs")

# ---- distinct_entity: added for the review (2026-10-27) -------------------
N("n54", "dev", "B0 round-1 adversarial review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 5 P3.",
  "B1 round-1 adversarial review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 4 P3.",
  O, f"{AR}adve-review-mooshik-B-B0-round1.md, adve-review-mooshik-B-B1-round1.md")
N("n55", "dev", "B2 round-1 adversarial review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 0 P3.",
  "B3 round-1 adversarial review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 0 P3.",
  O, f"{AR}adve-review-mooshik-B-B2-round1.md, adve-review-mooshik-B-B3-round1.md")
N("n56", "held_out", "B3 round-2 adversarial review verdict: APPROVE, the single closure holds.",
  "B4 round-2 adversarial review verdict: APPROVE, the single closure holds.",
  O, f"{AR}adve-review-mooshik-B-B3-round2.md, adve-review-mooshik-B-B4-round2.md")
N("n57", "dev", "B0 round-2 adversarial review verdict: APPROVE, all 6 closures hold.",
  "B1 round-2 adversarial review verdict: APPROVE, all 5 closures hold.",
  O, f"{AR}adve-review-mooshik-B-B0-round2.md, adve-review-mooshik-B-B1-round2.md")
N("n58", "held_out", "B end-to-end round-2 review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 5 P3.",
  "B end-to-end round-1 review verdict: REQUEST_CHANGES, 2 P1, 2 P2, 7 P3.",
  O, f"{AR}adve-review-mooshik-B-e2e-round1.md, adve-review-mooshik-B-e2e-round2.md")
N("n59", "dev", "B end-to-end round-3 review verdict: REQUEST_CHANGES, 0 P1, 0 P2, 1 P3.",
  "B end-to-end round-5 review verdict: REQUEST_CHANGES, 0 P1, 0 P2, 1 P3.",
  O, f"{AR}adve-review-mooshik-B-e2e-round3.md, adve-review-mooshik-B-e2e-round5.md")
N("n60", "dev", "J4 round-1 adversarial review verdict: APPROVE.",
  "J5 round-1 adversarial review verdict: APPROVE.",
  O, f"{AR}adve-review-mooshik-J4-round1.md, adve-review-mooshik-J5-round1.md")
N("n61", "held_out", "L1 round-1 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  "J3 round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  O, f"{AR}adve-review-mooshik-L1-round1.md, adve-review-mooshik-J3-round2.md")
N("n62", "dev", "A3 round-1 adversarial review verdict: REQUEST_CHANGES, 1 P1, 0 P2, 1 P3.",
  "B2 round-1 adversarial review verdict: REQUEST_CHANGES, 0 P1, 1 P2, 0 P3.",
  O, f"{AR}adve-review-mooshik-A-A3-round1.md, adve-review-mooshik-B-B2-round1.md")
N("n63", "held_out", "A2 round-3 adversarial review verdict: APPROVE, zero findings.",
  "A4 round-1 adversarial review verdict: APPROVE, zero findings.",
  O, f"{AR}adve-review-mooshik-A-A2-round3.md, adve-review-mooshik-A-A4-round1.md")
N("n64", "dev", "PR #183 merged to main as a182136a on 2026-10-19.",
  "PR #185 merged to main as a94942b2 on 2026-10-19.",
  O, "git log --merges (a182136a, a94942b2)")
N("n65", "dev", "PR #169 merged to main as 31b212f3 on 2026-10-14.",
  "PR #170 merged to main as 3d126d4c on 2026-10-14.",
  O, "git log --merges (31b212f3, 3d126d4c)")
N("n66", "held_out", "PR #166 merged to main as d12b09f4 on 2026-10-13.",
  "PR #167 merged to main as 3083df5a on 2026-10-13.",
  O, "git log --merges (d12b09f4, 3083df5a)")
N("n67", "dev", "PR #186 merged to main as a6ad3323 on 2026-10-19.",
  "PR #184 merged to main as f19940bb on 2026-10-19.",
  O, "git log --merges (a6ad3323, f19940bb)")
N("n68", "held_out", "On 0.4.0, lambo_recall with top_k 8 on the Metal dogfood rig runs at 5.7 ms p50.",
  "On 0.3.0, lambo_recall with top_k 8 on the Metal dogfood rig runs at 310.0 ms p50.",
  O, "evidence/issue-108-perf/README.md (Metal rig, 0.3.0 against 0.4.0)")
N("n69", "dev", "On 0.4.0, a lambo_derive on the CUDA dogfood rig goes from ack to applied in 165.3 ms p50.",
  "On bef53e6, a lambo_derive on the CUDA dogfood rig goes from ack to applied in 2,933.5 ms p50.",
  O, "evidence/issue-108-perf/README.md (CUDA rig, #108: bef53e6 against 0.4.0)")
N("n70", "dev", "One daemon rescore of a 5,000-concept session takes 8.0 ms.",
  "One daemon rescore of a 9,100-concept session takes 31.3 ms.",
  O, "evidence/issue-171-perf/README.md (rescore table, single walk)")
N("n71", "held_out", "Issue #8 shipped in lambo 0.4.0.",
  "Issue #14 shipped in lambo 0.4.0.",
  O, f"{CL} (0.4.0: #8, #14)")
N("n72", "dev", "Issue #157 shipped in lambo 0.6.0.",
  "Issue #36 shipped in lambo 0.6.0.",
  O, f"{CL} (0.6.0: #157, #36)")

# ---- same-event paraphrases (guard-refused, never a replacement) ----------
E("e01", "dev", "Merged PR #205 (#173 RRF fusion) to main as c3ba6722.",
  "#173's RRF fusion landed on main on 2026-10-27 through PR #205, merge c3ba6722.",
  O, "git log --merges (c3ba6722)")
E("e02", "held_out", "lambo 0.7.0 was released on 2026-10-19 through PR #189.",
  "PR #189 shipped release 0.7.0 (merge 21942f6f) on 2026-10-19.",
  O, f"{CL} (0.7.0); git log --merges (21942f6f)")
E("e03", "dev", "GC's first sweep on the Metal rig copy fell from 67.5 ms to 9.0 ms under the write lock.",
  "Under the write lock, the rig copy's first GC sweep now takes 9.0 ms, down from 67.5 ms.",
  O, f"{CL} (0.3.0, GC)")
E("e04", "held_out", "#87: under EmbeddingGemma 2 a writer merges at 0.93 instead of 0.85.",
  "Writers on the EG2 contract now merge at their calibrated 0.93, not the default 0.85 (#87).",
  L, f"{CL} (Unreleased, #87)")
E("e05", "dev", "On 2026-10-27 the #190 set gained 58 answerable queries: 36 vocabulary-mismatch, 9 rationale, 13 contrast.",
  "t_more173.py appended 58 answerable queries to the #190 set (36 vocabulary-mismatch, 9 rationale, 13 contrast) on 2026-10-27.",
  O, f"{EV} (Added for #173)")
E("e06", "dev", "0.5.0 applies in 5 s the replayed write call that 0.4.0 failed on its 30 s I/O deadline.",
  "0.4.0 failed on its 30 s I/O deadline the replayed write call that 0.5.0 applies in 5 s.",
  O, f"{CL} (0.5.0: same write outcomes as 0.4.0)")
E("e07", "dev", "The pinned Rust toolchain moved from 1.97.1 to 1.99.0 (#42).",
  "#42 raised the pinned toolchain to Rust 1.99.0 from 1.97.1.",
  C, f"{CL} (0.4.0, #42)")
E("e08", "held_out", "J3 redesign round-3 review: APPROVE, all nine round-2 findings closed at source at 9b4c456.",
  "At 9b4c456 the J3 redesign's round-3 review approved: the 9 round-2 findings are all closed at source.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round3.md")
