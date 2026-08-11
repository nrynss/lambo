#!/usr/bin/env python3
"""Generate Lambo v0.1 fixture graphs (T1.4). Run from repo root:

    python3 scripts/gen-fixtures.py

Writes deterministic, internally-consistent JSON into fixtures/ that load into
MemoryStore via `fixtures::load` and satisfy the P4/P5/P6 structural invariants,
including spec §5.7 (Temporal chain + Derives on every concept) and lawfully
passing canonization stages.

Canonical keys are computed here with the SAME convention as
`canonicalization-cases.json` (rust-stemmers Porter stems, probe-verified):
    lowercase -> split [-_ ] + camelCase -> drop stopwords -> Porter stem ->
    sort -> join " ".
A small stem table covers the closed fixture vocabulary; re-probe with
`rust-stemmers` if words are added.
"""
import json, os, re
from datetime import datetime, timedelta, timezone

OUT = os.path.join(os.path.dirname(__file__), "..", "fixtures")
os.makedirs(OUT, exist_ok=True)

def nid(n):
    return f"f0000000-0000-4000-8000-{n:012d}"

BASE = datetime(2026, 8, 10, 9, 0, 0, tzinfo=timezone.utc)
T = lambda m: (BASE + timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")

A, B = "agent-a", "agent-b"
SID = "session-rest-api"

# --- canonical keys ----------------------------------------------------------
# Porter (English) stems for the closed fixture vocabulary (probe-verified).
STEM = {
    "registering":"regist","register":"regist","registered":"regist",
    "users":"user","user":"user","systems":"system","system":"system",
    "connecting":"connect","connect":"connect","schema":"schema","schemas":"schema",
    "authentication":"authent","authorization":"author","validating":"valid",
    "validated":"valid","validation":"valid","rules":"rule",
    "creating":"creat","created":"creat","creat":"creat","create":"creat",
    "pagination":"pagin","paginate":"pagin","rate":"rate","limits":"limit",
    "limiter":"limit","limit":"limit","caching":"cach","cache":"cach",
    "documentation":"document","docs":"doc","doc":"doc",
    "password":"password","passwords":"password","reset":"reset","resetting":"reset",
    "logging":"log","loadtesting":"loadtest","testing":"test","load":"load",
    "id":"id","ratelimit":"ratelimit","registration":"registr","time":"time",
    "birth":"birth","join":"join","updated":"updat","update":"updat",
    "profile":"profil","middleware":"middlewar","response":"respons",
    "responses":"respons","launch":"launch","product":"product","path":"path",
    "step":"step","far":"far","budget":"budget","concept":"concept",
    "isolated":"isol","widget":"widget","sibling":"sibl","web":"web",
    "framework":"framework","database":"databas","layer":"layer",
    "authenticate":"authent","auth":"auth","role":"role","email":"email",
    "hash":"hash","status":"status","error":"error","api":"api",
    "account":"account","one":"one","two":"two","three":"three","four":"four",
    "five":"five",
}
STOPWORDS = {"the","a","an","for","of","at","in","to","on","and","or","is","are"}

def split_tokens(s):
    s = re.sub(r"[\-_]", " ", s.lower())
    # split camelCase ("UserSchema" -> "User Schema")
    s = re.sub(r"([a-z])([A-Z])", r"\1 \2", s)
    return [t for t in s.split() if t]

def key_of(content):
    toks = [t for t in split_tokens(content) if t not in STOPWORDS]
    stems = sorted(STEM.get(t, t) for t in toks)
    return " ".join(stems)

def ctype_for(name):
    return "Entity"

# --- session-rest-api --------------------------------------------------------
interactions = []
prev = None
for i in range(1, 13):
    m, ag, p = [
        (0,  A, "design the user schema for the REST API"),
        (5,  B, "review the user schema"),
        (10, A, "define the endpoint for creating users"),
        (15, B, "add auth middleware"),
        (20, A, "spec the validation rules"),
        (25, B, "implement the password reset flow"),
        (30, A, "design the rate limiter"),
        (35, B, "define the error responses"),
        (40, A, "add pagination to list endpoints"),
        (45, B, "write the api docs"),
        (50, A, "add a caching layer"),
        (55, B, "load test the endpoints"),
    ][i - 1]
    interactions.append({
        "id": nid(i), "session_id": SID, "agent_id": ag,
        "prompt_text": p, "previous_id": prev, "created_at": T(m),
    })
    prev = nid(i)

def concept(cid, content, origin_ix, agent, created_m, gc, status, blast=None):
    return {
        "id": nid(cid), "session_id": SID, "content": content,
        "canonical_key": key_of(content), "concept_type": ctype_for(content),
        "origin_interaction": nid(origin_ix), "origin_agent": agent,
        "created_at": T(created_m), "access_count": 0, "last_accessed": None,
        "gc_survived": gc, "canonization_status": status,
        "blast_radius": blast, "last_demotion_time": None, "embedding": None,
    }

US, CREATE, AUTH, VALID, RESET, RATE, ERROR = 1001, 1002, 1003, 1004, 1005, 1006, 1007
PAG, DOCS, CACHE, LOAD, API = 1008, 1009, 1010, 1011, 1012
D1, D2, D3, D4, D5, D6, D7, D8 = 1013, 1014, 1015, 1016, 1017, 1018, 1019, 1020
P1, P2 = 1021, 1022                       # extra non-Canonical peers for the Stage-1 gate

concepts = [
    concept(US,    "user schema",       1, A, 0,  4, "Canonical", 8),
    concept(CREATE,"create user",       3, A, 10, 2, "None"),
    concept(AUTH,  "auth middleware",   4, B, 15, 2, "None"),
    concept(VALID, "validation rules",  5, A, 20, 2, "None"),
    concept(RESET, "password reset",    6, B, 25, 2, "None"),
    concept(RATE,  "rate limiter",      7, A, 30, 2, "None"),
    concept(ERROR, "error responses",   8, B, 35, 2, "None"),
    concept(PAG,   "pagination",        9, A, 40, 1, "None"),
    concept(DOCS,  "api docs",          10, B, 45, 1, "None"),
    concept(CACHE, "caching layer",     11, A, 55, 0, "None"),
    concept(LOAD,  "load testing",      12, B, 55, 0, "None"),
    concept(API,   "api layer",         12, B, 55, 3, "Venerable", 1),
    concept(D1,    "user id",           1, A, 0,  0, "None"),
    concept(D2,    "user email",        2, B, 5,  0, "None"),
    concept(D3,    "user password hash",3, A, 10, 0, "None"),
    concept(D4,    "user role",         4, B, 15, 0, "None"),
    concept(D5,    "user status",       5, A, 20, 0, "None"),
    concept(D6,    "user profile",      6, B, 25, 0, "None"),
    concept(D7,    "user join time",    7, A, 30, 0, "None"),   # no "create" collision
    concept(D8,    "user updated time", 8, B, 35, 0, "None"),
    concept(P1,    "web framework",     3, A, 10, 0, "None"),
    concept(P2,    "database schema",   5, B, 20, 0, "None"),
]

def edge(eid, src, tgt, etype, w, m, reinf=1):
    return {"id": nid(3000 + eid), "session_id": SID, "source": nid(src),
            "target": nid(tgt), "edge_type": etype, "weight": w,
            "reinforcements": reinf, "created_at": T(m), "last_reinforced": T(m)}

edges = []
_e = [0]
def add_edge(src, tgt, etype, w, m):
    _e[0] += 1
    edges.append(edge(_e[0], src, tgt, etype, w, m))

# §5.7: every non-first interaction has exactly one Temporal predecessor (i -> prev)
for i in range(2, 13):
    add_edge(i, i - 1, "Temporal", 1.0, 5 * (i - 1))
# §5.7: every concept has at least one Derives edge (origin interaction -> concept)
for c in concepts:
    add_edge(int(c["origin_interaction"][-12:]), int(c["id"][-12:]), "Derives", 0.9, 5)

# supporters -> user schema (interaction_span: 6 distinct origins, span 25/55 ~0.455)
for s, tm in [(CREATE,10),(AUTH,15),(VALID,20),(RESET,25),(RATE,30),(ERROR,35)]:
    add_edge(s, US, "Dependency", 1.0, tm)
# user schema -> orphans (blast_radius = 8 > 5)
for o, tm in [(D1,0),(D2,5),(D3,10),(D4,15),(D5,20),(D6,25),(D7,30),(D8,35)]:
    add_edge(US, o, "Dependency", 0.8, tm)
# api layer supporters (Stage 2: 3 distinct origins, span 30/55 ~0.545)
for s, tm in [(CREATE,10),(RATE,30),(PAG,40)]:
    add_edge(s, API, "Dependency", 1.0, tm)
# api layer -> exclusive dependents (computed blast_radius = 1 <= 5, fails Stage 3)
add_edge(API, DOCS, "Dependency", 0.7, 45)
add_edge(API, CACHE, "Dependency", 0.7, 50)
# conflict seed: both agents touch caching layer (load-test from B, derive from A)
add_edge(LOAD, CACHE, "Dependency", 0.6, 55)

snapshot_rest = {
    "session_id": SID, "root_goal": None, "created_at": T(0), "closed_at": None,
    "interactions": interactions, "concepts": concepts, "edges": edges,
    "synonyms": [{"session_id": SID, "source_key": "register_user", "canonical_key": "create_user"}],
    "reservations": [], "canonization_events": [],
}

# --- session-drift ------------------------------------------------------------
DS = "session-drift"
D = lambda n: nid(5000 + n)
d_base = datetime(2026, 8, 11, 8, 0, 0, tzinfo=timezone.utc)
DT = lambda m: (d_base + timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")

def d_inter(i, m, prompt, prev=None):
    return {"id": D(i), "session_id": DS, "agent_id": A,
            "prompt_text": prompt, "previous_id": D(prev) if prev else None,
            "created_at": DT(m)}

d_interactions = [d_inter(1, 0, "layout the whole product plan"),
                  d_inter(2, 30, "isolate widget work", prev=1)]

def d_concept(idx, content, origin_ix, status="None", gc=0):
    return {"id": D(idx), "session_id": DS, "content": content,
            "canonical_key": key_of(content), "concept_type": "Entity",
            "origin_interaction": D(origin_ix), "origin_agent": A,
            "created_at": DT(20 if origin_ix == 1 else 40), "access_count": 0,
            "last_accessed": None, "gc_survived": gc,
            "canonization_status": status, "blast_radius": None,
            "last_demotion_time": None, "embedding": None}

d_concepts = [
    d_concept(10, "launch the product", 1, "Venerable", 5),   # root goal
    d_concept(11, "on path step one", 1, gc=2),
    d_concept(12, "on path step two", 1, gc=2),
    d_concept(13, "on path step three", 1, gc=2),
    d_concept(14, "on path step four", 1, gc=1),
    d_concept(15, "on path step five", 1, gc=1),
    d_concept(16, "far budget concept", 1, gc=1),   # 6 hops -> drift trigger
    d_concept(20, "isolated widget", 2, gc=0),      # disconnected component
    d_concept(21, "isolated sibling", 2, gc=0),
]

d_edges = []
def d_edge(etype, src_id, tgt_id, m, w=1.0):
    d_edges.append({"id": D(9000 + len(d_edges) + 1), "session_id": DS,
                    "source": D(src_id), "target": D(tgt_id), "edge_type": etype,
                    "weight": w, "reinforcements": 1,
                    "created_at": DT(m), "last_reinforced": DT(m)})

# §5.7 structure: i2 has Temporal predecessor i1 (indices 2 -> 1)
d_edge("Temporal", 2, 1, 5)
# §5.7: Derives (origin interaction -> concept) for every concept (indices)
for dc in d_concepts:
    oidx = 1 if dc["origin_interaction"] == D(1) else 2
    d_edge("Derives", oidx, int(dc["id"][-12:]) - 5000, 5, 0.9)
# drift chain (directed Dependency): goal -> ... -> far (far at 6 hops)
d_edge("Dependency", 10, 11, 5); d_edge("Dependency", 11, 12, 10)
d_edge("Dependency", 12, 13, 15); d_edge("Dependency", 13, 14, 20)
d_edge("Dependency", 14, 15, 25); d_edge("Dependency", 15, 16, 30)
# disconnected component (GC step 3 food)
d_edge("Dependency", 20, 21, 35)

snapshot_drift = {
    "session_id": DS, "root_goal": "launch the product",
    "created_at": DT(0), "closed_at": None,
    "interactions": d_interactions, "concepts": d_concepts, "edges": d_edges,
    "synonyms": [], "reservations": [], "canonization_events": [],
}

# --- mutations-batch (all five kinds, spec-legal edge types) ------------------
MB = "session-mutations"
M = lambda n: nid(7000 + n)
MT = T(0)
# nodes: interactions 7001 (first), 7004 (second); concepts 7002 (kept), 7003 (deleted)
mutations = [
    {"op": "upsert_node", "node": {"kind": "interaction", "id": M(1),
        "session_id": MB, "agent_id": A, "prompt_text": "mutate me",
        "previous_id": None, "created_at": MT}},
    {"op": "upsert_node", "node": {"kind": "interaction", "id": M(4),
        "session_id": MB, "agent_id": B, "prompt_text": "second step",
        "previous_id": M(1), "created_at": MT}},
    {"op": "upsert_node", "node": {"kind": "concept", "id": M(2),
        "session_id": MB, "content": "kept concept", "canonical_key": "concept kept",
        "concept_type": "Entity", "origin_interaction": M(1), "origin_agent": A,
        "created_at": MT, "access_count": 0, "last_accessed": None,
        "gc_survived": 1, "canonization_status": "None", "blast_radius": None,
        "last_demotion_time": None, "embedding": None}},
    {"op": "upsert_node", "node": {"kind": "concept", "id": M(3),
        "session_id": MB, "content": "deleted concept", "canonical_key": "concept deleted",
        "concept_type": "Observation", "origin_interaction": M(1), "origin_agent": B,
        "created_at": MT, "access_count": 0, "last_accessed": None,
        "gc_survived": 0, "canonization_status": "None", "blast_radius": None,
        "last_demotion_time": None, "embedding": None}},
    # edges with spec-legal endpoint types
    {"op": "upsert_edge", "edge": {"id": M(51), "session_id": MB,
        "source": M(1), "target": M(4), "edge_type": "Temporal", "weight": 1.0,
        "reinforcements": 1, "created_at": MT, "last_reinforced": MT}},   # I->I
    {"op": "upsert_edge", "edge": {"id": M(52), "session_id": MB,
        "source": M(1), "target": M(2), "edge_type": "Derives", "weight": 0.9,
        "reinforcements": 1, "created_at": MT, "last_reinforced": MT}},   # I->C
    {"op": "upsert_edge", "edge": {"id": M(53), "session_id": MB,
        "source": M(2), "target": M(3), "edge_type": "Dependency", "weight": 0.8,
        "reinforcements": 1, "created_at": MT, "last_reinforced": MT}},   # C->C
    # deletions (delete_edge targets the Temporal edge, untouched by delete_node(3))
    {"op": "delete_node", "id": M(3)},
    {"op": "delete_edge", "id": M(51)},
    {"op": "canonization_transition", "event": {"id": M(90),
        "session_id": MB, "node_id": M(2), "from_status": "None",
        "to_status": "Candidate", "blast_radius": None, "occurred_at": MT}},
]
mutations_batch = {"mutations": mutations}

# --- recall-goldens (phase1 EXACT under MemoryStore keyword_candidates) -------
recall_goldens = {
    "session": SID,
    "note": ("phase1_candidates are EXACT under MemoryStore keyword_candidates "
             "(substring on content/canonical_key). phase2_expanded lists REQUIRED "
             "members (candidate + direct neighbors); the full depth-2 set is P5's to "
             "compute. Assert membership + structural ordering, not floats."),
    "cases": [
        {"query": "pagination", "top_k": 5, "depth": 2,
         "phase1_candidates": [nid(PAG)],
         "phase2_expanded": [nid(PAG), nid(API)]},
        {"query": "create", "top_k": 5, "depth": 2,
         "phase1_candidates": [nid(CREATE)],
         "phase2_expanded": [nid(CREATE), nid(US), nid(API)]},
    ],
}

# --- canonicalization-cases (T6 contract) -------------------------------------
canon_cases = [
    {"category": "case",   "input": "User Schema", "expected_key": "schema user", "note": "lowercase + sort"},
    {"category": "hyphen", "input": "user-schema", "expected_key": "schema user", "note": "split hyphens"},
    {"category": "underscore", "input": "user_schema", "expected_key": "schema user", "note": "split underscores"},
    {"category": "camelcase", "input": "UserSchema", "expected_key": "schema user", "note": "split camelCase"},
    {"category": "stopword", "input": "the user schema api", "expected_key": "api schema user", "note": "strip 'the'"},
    {"category": "stem", "input": "registering users", "expected_key": "regist user", "note": "Porter stem"},
    {"category": "stem", "input": "creating cached systems", "expected_key": "cach creat system", "note": "multi-token stems"},
    {"category": "tokensort", "input": "schema the user", "expected_key": "schema user", "note": "sort + stopword"},
    {"category": "synonym", "input": "register_user", "expected_key": "creat user", "note": "register_user->create_user then canonical key"},
    {"category": "semantic-near-pair-A", "input": "register user", "expected_key": "regist user", "note": "distinct from B; hybrid step 6 merges"},
    {"category": "semantic-near-pair-B", "input": "create account", "expected_key": "account creat", "note": "distinct from A; hybrid step 6 merges"},
]

def dump(name, obj):
    p = os.path.join(OUT, name)
    with open(p, "w") as f:
        json.dump(obj, f, indent=2)
        f.write("\n")
    print(f"wrote {p}")

dump("session-rest-api.json", snapshot_rest)
dump("session-drift.json", snapshot_drift)
dump("mutations-batch.json", mutations_batch)
dump("recall-goldens.json", recall_goldens)
dump("canonicalization-cases.json", canon_cases)
print("done")
