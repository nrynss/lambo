#!/usr/bin/env python3
"""Generate Lambo v0.1 fixture graphs (T1.4). Run from repo root:

    python3 scripts/gen-fixtures.py

Writes deterministic, internally-consistent JSON into fixtures/ that load into
MemoryStore via `fixtures::load` and satisfy the P4/P5/P6 structural invariants.

IDs: interactions 1..12, concepts 1001.., edges 3001..  (stable hex uuids).
Timestamps are in the past relative to eval time, so age filters treat all edges
as aged.
"""
import json, os
from datetime import datetime, timedelta, timezone

OUT = os.path.join(os.path.dirname(__file__), "..", "fixtures")
os.makedirs(OUT, exist_ok=True)

def nid(n):
    return f"f0000000-0000-4000-8000-{n:012d}"

BASE = datetime(2026, 8, 10, 9, 0, 0, tzinfo=timezone.utc)
T = lambda m: (BASE + timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")

A, B = "agent-a", "agent-b"
SID = "session-rest-api"

# ---------------------------------------------------------------------------
# session-rest-api.json
# ---------------------------------------------------------------------------
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

def concept(cid, content, ckey, ctype, origin_ix, agent, created_m, gc, status,
            blast=None):
    return {
        "id": nid(cid), "session_id": SID, "content": content,
        "canonical_key": ckey, "concept_type": ctype,
        "origin_interaction": nid(origin_ix), "origin_agent": agent,
        "created_at": T(created_m), "access_count": 0, "last_accessed": None,
        "gc_survived": gc, "canonization_status": status,
        "blast_radius": blast, "last_demotion_time": None, "embedding": None,
    }

# concept ids (ints)
US, CREATE, AUTH, VALID, RESET, RATE, ERROR = 1001, 1002, 1003, 1004, 1005, 1006, 1007
PAG, DOCS, CACHE, LOAD, API = 1008, 1009, 1010, 1011, 1012
D1, D2, D3, D4, D5, D6, D7, D8 = 1013, 1014, 1015, 1016, 1017, 1018, 1019, 1020

concepts = [
    # main node: passes all three canonization stages
    concept(US, "user schema", "schema user", "Entity", 1, A, 0, 4, "Canonical", 8),
    # interaction-span supporters of user schema (origins i3..i8 => 6 distinct)
    concept(CREATE, "create user", "creat user", "Logic", 3, A, 10, 2, "None"),
    concept(AUTH, "auth middleware", "auth middleware", "Resource", 4, B, 15, 2, "None"),
    concept(VALID, "validation rules", "rule valid", "Constraint", 5, A, 20, 2, "None"),
    concept(RESET, "password reset", "password reset", "Logic", 6, B, 25, 2, "None"),
    concept(RATE, "rate limiter", "limit rate", "Resource", 7, A, 30, 2, "None"),
    concept(ERROR, "error responses", "error response", "Observation", 8, B, 35, 2, "None"),
    concept(PAG, "pagination", "pagin", "Logic", 9, A, 40, 1, "None"),
    concept(DOCS, "api docs", "api doc", "Resource", 10, B, 45, 1, "None"),
    concept(CACHE, "caching layer", "cach layer", "Resource", 11, A, 55, 0, "None"),
    concept(LOAD, "load testing", "load test", "Observation", 12, B, 55, 0, "None"),
    # Stage 2 but not Stage 3
    concept(API, "api layer", "api layer", "Entity", 12, B, 55, 3, "Venerable", 1),
    # blast-radius orphans (exclusive dependents of user schema)
    concept(D1, "user id", "id user", "Entity", 1, A, 0, 0, "None"),
    concept(D2, "user email", "email user", "Entity", 2, B, 5, 0, "None"),
    concept(D3, "user password hash", "hash password user", "Observation", 3, A, 10, 0, "None"),
    concept(D4, "user role", "role user", "Entity", 4, B, 15, 0, "None"),
    concept(D5, "user status", "status user", "Observation", 5, A, 20, 0, "None"),
    concept(D6, "user profile", "profile user", "Entity", 6, B, 25, 0, "None"),
    concept(D7, "user created at", "created user", "Observation", 7, A, 30, 0, "None"),
    concept(D8, "user updated at", "updated user", "Observation", 8, B, 35, 0, "None"),
]

def edge(eid, src, tgt, etype, w, m, reinf=1):
    return {
        "id": nid(3000 + eid), "session_id": SID, "source": nid(src),
        "target": nid(tgt), "edge_type": etype, "weight": w,
        "reinforcements": reinf, "created_at": T(m), "last_reinforced": T(m),
    }

edges = []
_e = [0]
def add_edge(src, tgt, etype, w, m):
    _e[0] += 1
    edges.append(edge(_e[0], src, tgt, etype, w, m))

# supporters -> user schema (interaction_span: 6 distinct origins, span 25/60 ~0.42)
for s, tm in [(CREATE,10),(AUTH,15),(VALID,20),(RESET,25),(RATE,30),(ERROR,35)]:
    add_edge(s, US, "Dependency", 1.0, tm)
# user schema -> orphans (blast_radius = 8 > 5)
for o, tm in [(D1,0),(D2,5),(D3,10),(D4,15),(D5,20),(D6,25),(D7,30),(D8,35)]:
    add_edge(US, o, "Dependency", 0.8, tm)
# api layer supporters (Stage 2: 3 distinct origins, span 30/60 = 0.5)
for s, tm in [(CREATE,10),(RATE,30),(PAG,40)]:
    add_edge(s, API, "Dependency", 1.0, tm)
# api layer -> exclusive dependents (blast_radius = 2 <= 5, fails Stage 3)
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

# ---------------------------------------------------------------------------
# session-drift.json — root goal (Venerable), on-path chain (<=5 hops), a
# >5-hop concept (drift), a disconnected component (GC food).
# ---------------------------------------------------------------------------
DS = "session-drift"
D = lambda n: nid(5000 + n)
d_base = datetime(2026, 8, 11, 8, 0, 0, tzinfo=timezone.utc)
DT = lambda m: (d_base + timedelta(minutes=m)).strftime("%Y-%m-%dT%H:%M:%SZ")

def d_inter(i, m, prompt):
    return {"id": D(i), "session_id": DS, "agent_id": A,
            "prompt_text": prompt, "previous_id": None, "created_at": DT(m)}

d_interactions = [d_inter(1, 0, "layout the whole product plan"),
                  d_inter(2, 30, "isolate widget work")]

def d_concept(idx, content, origin_ix, status="None", gc=0):
    return {"id": D(idx), "session_id": DS, "content": content,
            "canonical_key": content, "concept_type": "Entity",
            "origin_interaction": D(origin_ix), "origin_agent": A,
            "created_at": DT(20 if origin_ix == 1 else 40), "access_count": 0,
            "last_accessed": None, "gc_survived": gc,
            "canonization_status": status, "blast_radius": None,
            "last_demotion_time": None, "embedding": None}

def d_edge(ei, src, tgt, m):
    return {"id": D(9000 + ei), "session_id": DS, "source": D(src),
            "target": D(tgt), "edge_type": "Dependency", "weight": 1.0,
            "reinforcements": 1, "created_at": DT(m), "last_reinforced": DT(m)}

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
d_edges = [d_edge(1, 10, 11, 5), d_edge(2, 11, 12, 10), d_edge(3, 12, 13, 15),
           d_edge(4, 13, 14, 20), d_edge(5, 14, 15, 25), d_edge(6, 15, 16, 30),
           d_edge(7, 20, 21, 35)]

snapshot_drift = {
    "session_id": DS, "root_goal": "launch the product",
    "created_at": DT(0), "closed_at": None,
    "interactions": d_interactions, "concepts": d_concepts, "edges": d_edges,
    "synonyms": [], "reservations": [], "canonization_events": [],
}

# ---------------------------------------------------------------------------
# mutations-batch.json — every mutation kind in valid order
# ---------------------------------------------------------------------------
MB = "session-mutations"
M = lambda n: nid(7000 + n)
MT = T(0)  # any past timestamp

mutations = [
    {"op": "upsert_node", "node": {"kind": "interaction", "id": M(1),
        "session_id": MB, "agent_id": A, "prompt_text": "mutate me",
        "previous_id": None, "created_at": MT}},
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
    {"op": "upsert_edge", "edge": {"id": M(51), "session_id": MB,
        "source": M(1), "target": M(2), "edge_type": "Temporal", "weight": 1.0,
        "reinforcements": 1, "created_at": MT, "last_reinforced": MT}},
    {"op": "upsert_edge", "edge": {"id": M(52), "session_id": MB,
        "source": M(2), "target": M(3), "edge_type": "Dependency", "weight": 0.9,
        "reinforcements": 1, "created_at": MT, "last_reinforced": MT}},
    {"op": "delete_node", "id": M(3)},
    {"op": "delete_edge", "id": M(52)},
    {"op": "canonization_transition", "event": {"id": M(90),
        "session_id": MB, "node_id": M(2), "from_status": "None",
        "to_status": "Candidate", "blast_radius": None, "occurred_at": MT}},
]
mutations_batch = {"mutations": mutations}

# ---------------------------------------------------------------------------
# recall-goldens.json — structural expectations for session-rest-api
# ---------------------------------------------------------------------------
recall_goldens = {
    "session": SID,
    "note": ("phase1_candidates are EXACT. phase2_expanded lists REQUIRED members "
             "(candidate + direct neighbors); the full depth-2 expanded set is P5's to "
             "compute and may be larger. Assert membership + structural ordering, not "
             "float scores. IDs use the f0000000 placeholder family."),
    "cases": [
        {"query": "pagination", "top_k": 5, "depth": 2,
         "phase1_candidates": [nid(PAG)],
         "phase2_expanded": [nid(PAG), nid(API)]},
        {"query": "create", "top_k": 5, "depth": 2,
         "phase1_candidates": [nid(CREATE)],
         "phase2_expanded": [nid(CREATE), nid(US), nid(API)]},
    ],
}

# ---------------------------------------------------------------------------
# canonicalization-cases.json — canonical-key table (T6 contract). Stems computed
# with rust-stemmers Porter (spec §6.3). Convention: lowercase -> split [-_ ] and
# camelCase -> drop stopwords -> Porter stem -> sort -> join " ".
# Synonym lookup on the raw normalized key before stemming: "register_user"
# -> "create_user" -> stem "create user" -> "creat user".
# ---------------------------------------------------------------------------
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
