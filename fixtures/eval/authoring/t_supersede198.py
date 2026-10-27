"""#198: supersession pairs and the trigger's hard negatives.

#198's suspected-supersession trigger looks at a vector hit the #195
identifier guard refused, and keeps it only when both concepts have the same
type (Logic, Constraint or Observation), the same repo_scope, and
near-identical text once identifiers and digits are masked. These pairs give
it something to be measured on (precision >= 0.90 per embedder):

Every call names its pair's id (`"s01"` is p-198-s01), so an id never
moves when a pair is added, removed or reordered.

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
       a paraphrase, never a replacement.

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


def S(key, newer, older, t, src, review=None):
    P(newer, older, "related_distinct", "identifier", src, relation="supersedes",
      review=review, types=(t, t), id=_id("s", key), labelled_on=LABELLED_ON,
      labeller=LABELLER)


def N(key, a, b, t, src, review=None):
    P(a, b, "related_distinct", "identifier", src, relation="distinct_entity",
      review=review, types=(t, t), id=_id("n", key), labelled_on=LABELLED_ON,
      labeller=LABELLER)


def E(key, a, b, t, src):
    P(a, b, "paraphrase", "same-event", src, types=(t, t), id=_id("e", key),
      labelled_on=LABELLED_ON, labeller=LABELLER)


CL = "CHANGELOG.md"
DF = "dev-diary/lambo-for-mooshik/DOGFOOD.md"
AR = "dev-diary/adversarial-review/"
EV = "fixtures/eval/README.md"

# ---- supersedes: pins ----------------------------------------------------
S("s01", "The Mac dogfood rig serves the pinned binary lambo-21e4cf8, built with store-sqlite,embed-candle-metal,embed-bge.",
  "The Mac dogfood rig serves the pinned binary lambo-19f51d3, built with store-sqlite,embed-candle-metal,embed-bge.",
  L, f"{DF} (open question 1: pin 21e4cf8; superseded pins 3039b82, then 19f51d3)")
S("s02", "Current Mac rig pin: 19f51d3, the binary every client's MCP registration spawns.",
  "Current Mac rig pin: 3039b82, the binary every client's MCP registration spawns.",
  L, f"{DF} (open question 1: superseded pins 3039b82, then 19f51d3)")
S("s03", "The Linux dogfood rig (CUDA build) is pinned to bbef4b3.",
  "The Linux dogfood rig (CUDA build) is pinned to fe4d63d.",
  L, f"{DF} (Linux rig, 2026-08-24: re-pinned fe4d63d to bbef4b3)")
S("s04", "The dogfood writer and portal run the pinned lambo 0.6.0 binary.",
  "The dogfood writer and portal run the pinned lambo 0.5.0 binary.",
  L, "evidence/0.6.0-vs-0.5.0/README.md (Builds; Re-pin)")
S("s05", "The dogfood writer runs the pinned lambo 0.5.0 release.",
  "The dogfood writer runs the pinned lambo 0.4.0 release.",
  L, "evidence/0.5.0-vs-0.4.0/README.md (the 0.5.0 re-pin); evidence/0.6.0-vs-0.5.0/README.md (0.5.0 is the rig's pinned binary)")

# ---- supersedes: toolchain, constants, defaults --------------------------
S("s06", "The pinned Rust toolchain is 1.99.0; CI and the release workflow install it directly.",
  "The pinned Rust toolchain is 1.97.1; CI and the release workflow install it directly.",
  C, f"{CL} (0.4.0: the pinned toolchain moves from 1.97.1 to 1.99.0, #42)")
S("s07", "The lambo crate is built on Rust edition 2024.",
  "The lambo crate is built on Rust edition 2021.",
  C, f"{CL} (0.4.0: the crate moves from edition 2021 to edition 2024, #42)")
S("s08", "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 18.",
  "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 17.",
  C, "src/store/batch.rs (CONCEPT_COLUMNS = 18); CHANGELOG.md 0.3.0 (16 to 17)")
S("s09", "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 17.",
  "lambo::store::batch::CONCEPT_COLUMNS, the bind-parameter count of one concept row, is 16.",
  C, f"{CL} (0.3.0: CONCEPT_COLUMNS 16 to 17)")
S("s10", "lambo::store::batch::INTERACTION_COLUMNS, the bind-parameter count of one interaction row, is 7.",
  "lambo::store::batch::INTERACTION_COLUMNS, the bind-parameter count of one interaction row, is 6.",
  C, f"{CL} (0.3.0: INTERACTION_COLUMNS 6 to 7)")
S("s11", "lambo::store::batch::EDGE_COLUMNS, the bind-parameter count of one edge row, is 10.",
  "lambo::store::batch::EDGE_COLUMNS, the bind-parameter count of one edge row, is 9.",
  C, f"{CL} (0.3.0: EDGE_COLUMNS 9 to 10)")
S("s12", "The longest lambo_stats wait_ms a caller may ask for is 34000.",
  "The longest lambo_stats wait_ms a caller may ask for is 4000.",
  C, f"{CL} (0.4.0: the longest wait_ms is now 34000, was 4000, #11)")
S("s13", "Under the EmbeddingGemma 2 contract, the recent-interactions recall leg scores 0.63.",
  "Under the EmbeddingGemma 2 contract, the recent-interactions recall leg scores 0.35.",
  L, f"{CL} (Unreleased, #87: the recent leg is calibrated per contract)")
S("s14", "A writer attaching under the EmbeddingGemma 2 contract merges at a semantic match threshold of 0.93.",
  "A writer attaching under the EmbeddingGemma 2 contract merges at a semantic match threshold of 0.85.",
  L, f"{CL} (Unreleased, #87: EG2 writers merge at 0.93, not 0.85)")
S("s15", "Under EmbeddingGemma 2, a layer whose min_similarity is unset takes a floor of 0.72.",
  "Under EmbeddingGemma 2, a layer whose min_similarity is unset takes a floor of 0.50.",
  L, f"{CL} (Unreleased, #87: an unset EG2 layer floor is 0.72, not 0.50)")
S("s16", "embed-candle-cuda builds against CUDA 13.4 directly, with no version override.",
  "embed-candle-cuda needs CUDARC_CUDA_VERSION=13030 set to build against CUDA 13.4.",
  C, f"{CL} (0.5.0: CUDA 13.4 builds directly, #111)")
S("s17", "The latest published lambo release is 0.7.0 (2026-10-19).",
  "The latest published lambo release is 0.6.0 (2026-10-13).",
  O, f"{CL} (release headings 0.6.0 and 0.7.0)")

# ---- supersedes: unscoped current measurements ---------------------------
S("s18", "Keyword recall over MCP on a 9,500-concept SQLite store runs at 7.5 ms p50.",
  "Keyword recall over MCP on a 9,500-concept SQLite store runs at 18.7 ms p50.",
  O, f"{CL} (0.7.0, #171); evidence/issue-171-perf/")
S("s19", "With 16 concurrent clients, a 9,500-concept SQLite store serves 1,104 recalls per second.",
  "With 16 concurrent clients, a 9,500-concept SQLite store serves 63 recalls per second.",
  O, f"{CL} (0.7.0, #171); evidence/issue-171-perf/")
S("s20", "A back-to-back lambo_derive on a 9,500-concept SQLite store is acknowledged in 0.8 ms p50.",
  "A back-to-back lambo_derive on a 9,500-concept SQLite store is acknowledged in 24.8 ms p50.",
  O, f"{CL} (0.7.0, #171)")
S("s21", "A back-to-back lambo_derive on a 9,500-concept SQLite store is applied in 6.3 ms p50.",
  "A back-to-back lambo_derive on a 9,500-concept SQLite store is applied in 32.4 ms p50.",
  O, f"{CL} (0.7.0, #171)")
S("s22", "A clean lambo provision of a 9,508-concept store takes 5.6 ms p50.",
  "A clean lambo provision of a 9,508-concept store takes 143 ms p50.",
  O, f"{CL} (0.7.0, #171: provision skips the #148 re-key scan once it has run clean)")
S("s23", "One daemon rescore of a 9,100-concept session takes 31 ms.",
  "One daemon rescore of a 9,100-concept session takes 201 ms.",
  O, f"{CL} (0.7.0, #171: a rescore walks incident edges once)")
S("s24", "A structural recall on a synthetic 9,100-concept store takes about 1 ms per query (release build).",
  "A structural recall on a synthetic 9,100-concept store takes about 70 ms per query (release build).",
  O, f"{CL} (0.7.0, #165)")
S("s25", "A derive commit holds the session write lock for 16.5 µs at 10,000 concepts (release build).",
  "A derive commit holds the session write lock for 6.8 ms at 10,000 concepts (release build).",
  O, f"{CL} (0.5.0, #102)")
S("s26", "A derive commit holds the session write lock for 21 µs at 100,000 concepts (release build).",
  "A derive commit holds the session write lock for 168 ms at 100,000 concepts (release build).",
  O, f"{CL} (0.5.0, #102)")
S("s27", "On Metal, the embedder of a lambo serve that becomes a proxy takes about 4 MB.",
  "On Metal, the embedder of a lambo serve that becomes a proxy takes about 1.4 GB.",
  O, f"{CL} (0.5.0, #31)")
S("s28", "Warm recall p50 on a 3,600-concept SQLite session is 4.3 ms.",
  "Warm recall p50 on a 3,600-concept SQLite session is 219 ms.",
  O, f"{CL} (0.4.0, #8, #14: the session holder ranks vectors from memory)")
S("s29", "A 3,600-concept SQLite store at dim 1024 takes 17.1 MB on disk.",
  "A 3,600-concept SQLite store at dim 1024 takes 47.3 MB on disk.",
  O, f"{CL} (0.7.0, #55: packed f32 vectors)",
  review="the two values are #55's text and packed codecs of one store; a store not yet provisioned on 0.7.0 still measures the older value, so this may be two codecs (distinct_entity) rather than a replacement")
S("s30", "Decoding every vector of a 3,600-concept SQLite store at dim 1024 takes 3.4 ms.",
  "Decoding every vector of a 3,600-concept SQLite store at dim 1024 takes 161 ms.",
  O, f"{CL} (0.7.0, #55)",
  review="the two values are #55's text and packed codecs of one store; a store not yet provisioned on 0.7.0 still measures the older value, so this may be two codecs (distinct_entity) rather than a replacement")
S("s31", "On a copy of the Metal rig store, GC's first sweep collects 0 concepts.",
  "On a copy of the Metal rig store, GC's first sweep collects 537 concepts.",
  O, f"{CL} (0.3.0, GC: the first sweep now collects nothing, was 537)")
S("s32", "GC's first sweep on the Metal rig copy holds the write lock for 9.0 ms.",
  "GC's first sweep on the Metal rig copy holds the write lock for 67.5 ms.",
  O, f"{CL} (0.3.0, GC: sweeps are cheap)")
S("s33", "In the REST API fixture, user schema has 14 dependents.",
  "In the REST API fixture, user schema has 8 dependents.",
  O, f"{CL} (0.4.0: user schema's eight fields are parent_of children, 14 dependents, was 8)")
S("s34", "The demo's user schema reports 15 dependents.",
  "The demo's user schema reports 9 dependents.",
  O, f"{CL} (0.4.0: the demo's user schema reports 15, was 9)")

# ---- supersedes: counts of this set (git history of the README) -----------
S("s35", "The #190 evaluation set holds 368 queries.",
  "The #190 evaluation set holds 310 queries.",
  O, f"{EV} (git 3d421739: 310 to 368 queries)")
S("s36", "The #190 set has 262 answerable queries.",
  "The #190 set has 204 answerable queries.",
  O, f"{EV} (git 3d421739: 204 to 262 answerable)")
S("s37", "The #190 queries split 239 dev / 129 held-out.",
  "The #190 queries split 199 dev / 111 held-out.",
  O, f"{EV} (git 3d421739)")
S("s38", "The #190 set has 48 vocabulary-mismatch queries.",
  "The #190 set has 12 vocabulary-mismatch queries.",
  O, f"{EV} (git 3d421739: 12 to 48)")
S("s39", "The #190 set has 17 rationale queries.",
  "The #190 set has 8 rationale queries.",
  O, f"{EV} (git 3d421739: 8 to 17)")
S("s40", "The #190 set has 36 contrast queries.",
  "The #190 set has 23 contrast queries.",
  O, f"{EV} (git 3d421739: 23 to 36)")
S("s41", "Of the #190 related-distinct pairs, 82 carry the relation contradicts.",
  "Of the #190 related-distinct pairs, 84 carry the relation contradicts.",
  O, f"{EV} (git 8d2269a9: the owner's rulings, 84 to 82)")

# ---- distinct_entity: review rounds (dev-diary/adversarial-review) --------
N("n01", "J1 round-1 adversarial review verdict: REQUEST_CHANGES with one P1.",
  "J2 round-1 adversarial review verdict: REQUEST_CHANGES with one P1.",
  O, f"{AR}adve-review-mooshik-J1-round1.md, adve-review-mooshik-J2-round1.md")
N("n02", "J3 round-1 adversarial review verdict: REQUEST_CHANGES, 1 P1, 4 P2, 8 P3.",
  "J3 round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  O, f"{AR}adve-review-mooshik-J3-round1.md, adve-review-mooshik-J3-round2.md")
N("n03", "J3 round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  "J3 redesign round-2 adversarial review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 5 P3.",
  O, f"{AR}adve-review-mooshik-J3-round2.md, adve-review-mooshik-J3-redesign-round2.md")
N("n04", "J3 round-3 adversarial review verdict: REQUEST_CHANGES with 2 new P1, 1 P2 and 3 P3.",
  "J3 round-2 adversarial review verdict: REQUEST_CHANGES with 1 new P1, 3 P2 and 6 P3.",
  O, f"{AR}adve-review-mooshik-J3-round2.md, adve-review-mooshik-J3-round3.md")
N("n05", "J1 round-2 adversarial review verdict: REQUEST_CHANGES, 0 P1, 2 P2 and 2 P3.",
  "J1 round-3 adversarial review verdict: CLEAN, 0 P1, 0 P2 and 3 P3.",
  O, f"{AR}adve-review-mooshik-J1-round2.md, adve-review-mooshik-J1-round3.md")
N("n06", "J0 round-1 adversarial review verdict: CLEAN, 0 P2 and 6 P3 advisory findings, none blocking.",
  "J0 round-2 adversarial review verdict: CLEAN, 1 P2 and 5 P3 advisory findings, none blocking.",
  O, f"{AR}adve-review-mooshik-J0-round1.md, adve-review-mooshik-J0-round2.md")
N("n07", "J3 redesign round-1 review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 6 P3.",
  "J3 redesign round-2 review verdict: REQUEST_CHANGES, 1 P1, 3 P2, 5 P3.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round1.md, adve-review-mooshik-J3-redesign-round2.md")
N("n08", "J end-to-end round-1 review verdict: REQUEST_CHANGES, 0 P1, 5 P2, 7 P3.",
  "J end-to-end round-2 review verdict: REQUEST_CHANGES, 0 P1, 0 P2, 7 P3.",
  O, f"{AR}adve-review-mooshik-J-e2e-round1.md, adve-review-mooshik-J-e2e-round2.md")
N("n09", "J2 round-3 adversarial review verdict: CLEAN.",
  "J1 round-3 adversarial review verdict: CLEAN.",
  O, f"{AR}adve-review-mooshik-J1-round3.md, adve-review-mooshik-J2-round3.md")
N("n10", "J3 redesign round-3 review verdict: APPROVE.",
  "J3 redesign round-2 review verdict: REQUEST_CHANGES.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round2.md, adve-review-mooshik-J3-redesign-round3.md")
N("n11", "Review notes for J2 round 1 are in adve-review-mooshik-J2-round1.md.",
  "Review notes for J2 round 2 are in adve-review-mooshik-J2-round2.md.",
  O, f"{AR} (one file per round; #178's real pairs i3-07, i3-17)")
N("n12", "Review notes for J3 round 1 are in adve-review-mooshik-J3-round1.md.",
  "Review notes for J3 round 3 are in adve-review-mooshik-J3-round3.md.",
  O, f"{AR} (one file per round; #178's real pair i3-05)")

# ---- distinct_entity: version-scoped measurements -------------------------
N("n13", "On 0.7.0, keyword recall over MCP on a 9,500-concept SQLite store runs at 7.5 ms p50.",
  "On 0.6.0, keyword recall over MCP on a 9,500-concept SQLite store runs at 18.7 ms p50.",
  O, f"{CL} (0.7.0, #171)")
N("n14", "On 0.7.0, 16 concurrent clients get 1,104 recalls per second from a 9,500-concept store.",
  "On 0.6.0, 16 concurrent clients get 63 recalls per second from a 9,500-concept store.",
  O, f"{CL} (0.7.0, #171)")
N("n15", "0.7.0 acknowledges a back-to-back lambo_derive in 0.8 ms p50.",
  "0.6.0 acknowledges a back-to-back lambo_derive in 24.8 ms p50.",
  O, f"{CL} (0.7.0, #171)")
N("n16", "On 0.7.0 a structural recall on a 9,100-concept store takes about 1 ms.",
  "On 0.6.0 a structural recall on a 9,100-concept store takes about 70 ms.",
  O, f"{CL} (0.7.0, #165)")
N("n17", "At 10,000 concepts a derive commit holds the write lock for 16.5 µs (0.5.0).",
  "At 50,000 concepts a derive commit holds the write lock for 17 µs (0.5.0).",
  O, f"{CL} (0.5.0, #102)")
N("n18", "At 50,000 concepts a derive commit holds the write lock for 17 µs.",
  "At 100,000 concepts a derive commit holds the write lock for 21 µs.",
  O, f"{CL} (0.5.0, #102)")
N("n19", "With RRF fusion, whole-pipeline nDCG@10 on the #190 dev split under BGE-M3 is 0.893.",
  "Without RRF fusion, whole-pipeline nDCG@10 on the #190 dev split under BGE-M3 is 0.848.",
  O, f"{CL} (Unreleased, #173)")
N("n20", "With RRF fusion, nDCG@10 on the #190 dev split is 0.893 under BAAI/bge-m3.",
  "With RRF fusion, nDCG@10 on the #190 dev split is 0.889 under google/embeddinggemma-2.",
  O, f"{CL} (Unreleased, #173)")
N("n21", "With RRF fusion, nDCG@10 under BGE-M3 is 0.893 on the #190 dev split.",
  "With RRF fusion, nDCG@10 under BGE-M3 is 0.887 on the #190 held_out split.",
  O, f"{CL} (Unreleased, #173)")
N("n22", "Without fusion, nDCG@10 under BGE-M3 is 0.848 on the #190 dev split.",
  "Without fusion, nDCG@10 under BGE-M3 is 0.865 on the #190 held_out split.",
  O, f"{CL} (Unreleased, #173)")

# ---- distinct_entity: per-embedder calibrations (#87) ---------------------
N("n23", "Under google/embeddinggemma-2 the recent-interactions leg scores 0.63.",
  "Under BAAI/bge-m3 the recent-interactions leg scores 0.35.",
  L, f"{CL} (Unreleased, #87)")
N("n24", "Under google/embeddinggemma-2 a writer merges at a threshold of 0.93.",
  "Under BAAI/bge-m3 a writer merges at a threshold of 0.85.",
  L, f"{CL} (Unreleased, #87)")
N("n25", "Under google/embeddinggemma-2 an unset layer floor is 0.72.",
  "Under BAAI/bge-m3 an unset layer floor is 0.50.",
  L, f"{CL} (Unreleased, #87)")
N("n26", "At a 0.50 layer floor, 99.9% of irrelevant concepts on the #190 held-out split pass under google/embeddinggemma-2.",
  "At a 0.50 layer floor, 3.2% of irrelevant concepts on the #190 held-out split pass under BAAI/bge-m3.",
  O, f"{CL} (Unreleased, #87); evidence/issue-87-calibration/")
N("n27", "At the 0.85 threshold, about 78% of the #190 related-but-distinct pairs are over it by cosine under google/embeddinggemma-2.",
  "At the 0.85 threshold, about 35-44% of the #190 related-but-distinct pairs are over it by cosine under BAAI/bge-m3.",
  O, f"{CL} (Unreleased, #87)")

# ---- distinct_entity: constants and caps that share a template ------------
N("n28", "lambo::store::batch::CONCEPT_COLUMNS is 18.",
  "lambo::store::batch::EDGE_COLUMNS is 10.",
  C, "src/store/batch.rs")
N("n29", "lambo::store::batch::INTERACTION_COLUMNS is 7 (src/store/batch.rs).",
  "lambo::store::batch::EDGE_COLUMNS is 10 (src/store/batch.rs).",
  C, "src/store/batch.rs")
N("n30", "At most 16 lambo_stats waits run at once per server.",
  "At most 8 lambo_stats waits run at once per agent_id.",
  C, f"{CL} (0.4.0, #11: wait caps)")
N("n31", "lambo::graph::hybrid::MAX_HYBRID_CONCEPTS is 256 (src/graph/hybrid.rs).",
  "lambo::graph::hybrid::MAX_HYBRID_WORK_ITEMS is 512 (src/graph/hybrid.rs).",
  C, "src/graph/hybrid.rs")

# ---- distinct_entity: rigs, releases, merges, milestones ------------------
N("n32", "The Mac dogfood rig is pinned to 21e4cf8 (candle on Metal).",
  "The Linux dogfood rig is pinned to bbef4b3 (candle on CUDA).",
  L, f"{DF} (open question 1, both rigs as of 2026-08-24)")
N("n33", "lambo 0.7.0 was released on 2026-10-19.",
  "lambo 0.6.0 was released on 2026-10-13.",
  O, f"{CL} (release headings)")
N("n34", "lambo 0.5.0 was released on 2026-10-12.",
  "lambo 0.4.0 was released on 2026-10-11.",
  O, f"{CL} (release headings)")
N("n35", "Release 0.3.0 of lambo is dated 2026-10-07 in CHANGELOG.md.",
  "Release 0.4.0 of lambo is dated 2026-10-11 in CHANGELOG.md.",
  O, f"{CL} (release headings)")
N("n36", "PR #205 merged to main as c3ba6722 on 2026-10-27.",
  "PR #206 merged to main as 1ac09bf8 on 2026-10-27.",
  O, "git log --merges (c3ba6722, 1ac09bf8)")
N("n37", "PR #203 merged to main as 266f8bea on 2026-10-26.",
  "PR #204 merged to main as 8c92a5e9 on 2026-10-26.",
  O, "git log --merges (266f8bea, 8c92a5e9)")
N("n38", "PR #199 merged to main as b8177774 on 2026-10-20.",
  "PR #200 merged to main as 437d3d58 on 2026-10-20.",
  O, "git log --merges (b8177774, 437d3d58)")
N("n39", "PR #195 merged to main as 5e44c692 on 2026-10-20.",
  "PR #194 merged to main as 3a76def7 on 2026-10-20.",
  O, "git log --merges (5e44c692, 3a76def7)")
N("n40", "Issue #198 is planned for milestone 0.8.0.",
  "Issue #181 is planned for milestone 0.8.0.",
  L, "nrynss/lambo issues #181, #198 (milestone 0.8.0)")
N("n41", "Issue #207 is planned for milestone 0.8.0.",
  "Issue #208 is planned for milestone 0.8.0.",
  L, "nrynss/lambo issues #207, #208 (milestone 0.8.0)")
N("n42", "Issue #117 belongs to milestone 0.5.0.",
  "Issue #118 belongs to milestone 0.5.0.",
  L, "nrynss/lambo issues #117, #118 (milestone 0.5.0); CHANGELOG.md 0.5.0")
N("n43", "Issue #180 belongs to milestone 0.8.0.",
  "Issue #116 belongs to milestone 0.5.0.",
  L, "nrynss/lambo issues #116 (0.5.0), #180 (0.8.0)")
N("n44", "Issue #176 shipped in lambo 0.7.0.",
  "Issue #178 shipped in lambo 0.7.0.",
  O, f"{CL} (0.7.0 highlights)")
N("n45", "Issue #55 shipped in lambo 0.7.0.",
  "Issue #165 shipped in lambo 0.7.0.",
  O, f"{CL} (0.7.0 performance)")
N("n46", "Issue #102 shipped in lambo 0.5.0.",
  "Issue #31 shipped in lambo 0.5.0.",
  O, f"{CL} (0.5.0)")
N("n47", "The dogfood writer listens on 127.0.0.1:7700.",
  "The old llama-server embedder listened on 127.0.0.1:8080.",
  O, "dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md (MCP url on 7700); DOGFOOD.md (bge_m3 at 127.0.0.1:8080)")
N("n48", "Phase 3 of the build is written up in dev-diary/PHASE-3-stores.md.",
  "Phase 4 of the build is written up in dev-diary/PHASE-4-daemon.md.",
  O, "dev-diary/PHASE-3-stores.md, dev-diary/PHASE-4-daemon.md")
N("n49", "Phase 5 of the build is written up in dev-diary/PHASE-5-recall.md.",
  "Phase 6 of the build is written up in dev-diary/PHASE-6-canonization.md.",
  O, "dev-diary/PHASE-5-recall.md, dev-diary/PHASE-6-canonization.md")
N("n50", "PR #188 merged to main as d327bf77 on 2026-10-19.",
  "PR #187 merged to main as d38c81ee on 2026-10-19.",
  O, "git log --merges (d327bf77, d38c81ee)")
N("n51", "Issue #209 belongs to milestone 0.8.0.",
  "Issue #181 belongs to milestone 0.8.0.",
  L, "nrynss/lambo issues #181, #209 (milestone 0.8.0)")
N("n52", "lambo::graph::hybrid::MAX_HYBRID_PARENT_PAIRS is 256.",
  "lambo::graph::hybrid::MAX_HYBRID_WORK_ITEMS is 512.",
  C, "src/graph/hybrid.rs")
N("n53", "lambo::surface::limits::MAX_CONTENT_BYTES is 16,384 bytes.",
  "lambo::graph::hybrid::MAX_HYBRID_CONTEXT_BYTES is 16 * 1024 bytes.",
  C, "src/surface/limits.rs; src/graph/hybrid.rs")

# ---- same-event paraphrases (guard-refused, never a replacement) ----------
E("e01", "Merged PR #205 (#173 RRF fusion) to main as c3ba6722.",
  "#173's RRF fusion landed on main on 2026-10-27 through PR #205, merge c3ba6722.",
  O, "git log --merges (c3ba6722)")
E("e02", "lambo 0.7.0 was released on 2026-10-19 through PR #189.",
  "PR #189 shipped release 0.7.0 (merge 21942f6f) on 2026-10-19.",
  O, f"{CL} (0.7.0); git log --merges (21942f6f)")
E("e03", "GC's first sweep on the Metal rig copy fell from 67.5 ms to 9.0 ms under the write lock.",
  "Under the write lock, the rig copy's first GC sweep now takes 9.0 ms, down from 67.5 ms.",
  O, f"{CL} (0.3.0, GC)")
E("e04", "#87: under EmbeddingGemma 2 a writer merges at 0.93 instead of 0.85.",
  "Writers on the EG2 contract now merge at their calibrated 0.93, not the default 0.85 (#87).",
  L, f"{CL} (Unreleased, #87)")
E("e05", "On 2026-10-27 the #190 set gained 58 answerable queries: 36 vocabulary-mismatch, 9 rationale, 13 contrast.",
  "t_more173.py appended 58 answerable queries to the #190 set (36 vocabulary-mismatch, 9 rationale, 13 contrast) on 2026-10-27.",
  O, f"{EV} (Added for #173)")
E("e06", "0.5.0 applies in 5 s the replayed write call that 0.4.0 failed on its 30 s I/O deadline.",
  "0.4.0 failed on its 30 s I/O deadline the replayed write call that 0.5.0 applies in 5 s.",
  O, f"{CL} (0.5.0: same write outcomes as 0.4.0)")
E("e07", "The pinned Rust toolchain moved from 1.97.1 to 1.99.0 (#42).",
  "#42 raised the pinned toolchain to Rust 1.99.0 from 1.97.1.",
  C, f"{CL} (0.4.0, #42)")
E("e08", "J3 redesign round-3 review: APPROVE, all nine round-2 findings closed at source at 9b4c456.",
  "At 9b4c456 the J3 redesign's round-3 review approved: the 9 round-2 findings are all closed at source.",
  O, f"{AR}adve-review-mooshik-J3-redesign-round3.md")
