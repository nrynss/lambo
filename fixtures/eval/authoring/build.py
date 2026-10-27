#!/usr/bin/env python3
"""Assemble fixtures/eval/{corpus,queries,pairs,related_pairs}.json from the
authoring modules in this directory. Deterministic: rerunning it on the same
inputs writes byte-identical files, and tests/eval_set.rs checks the export
against pairs.json.

    python3 fixtures/eval/authoring/build.py
"""
import hashlib, importlib, json, random, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE.parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(HERE))
import common  # noqa: E402

MODULES = ["t_embed", "t_serve", "t_release", "t_findings", "t_paper", "t_noanswer", "t_para",
           "t_more173", "t_supersede198"]
CONCEPT_TYPES = ("Entity", "Logic", "Constraint", "Resource", "Observation")
LABELLER = "Grok Bot (agent)"
LABELLED_ON = "2026-10-23"
HELD_OUT_PERCENT = 30
UNRELATED_PAIRS = 120

for m in MODULES:
    importlib.import_module(m)


def split_of(key):
    """Deterministic dev/held-out split, about 70/30, by a hash of the id."""
    h = int(hashlib.sha256(key.encode()).hexdigest()[:8], 16)
    return "held_out" if h % 100 < HELD_OUT_PERCENT else "dev"


def norm(q):
    """(text, family, extra grades, labelled_on, facets or None)."""
    if isinstance(q, str):
        return q, "fact-lookup", {}, LABELLED_ON, None
    if len(q) == 2:
        return q[0], q[1], {}, LABELLED_ON, None
    if len(q) == 3:
        return q[0], q[1], q[2], LABELLED_ON, None
    return q


ids = [c["id"] for c in common.CONCEPTS]
assert len(ids) == len(set(ids)), "duplicate concept id"
known = set(ids)

# ---- corpus -------------------------------------------------------------
corpus = [dict(id=c["id"], topic=c["topic"], type=c["type"], text=c["text"],
               origin=c["origin"], sources=c["sources"]) for c in common.CONCEPTS]
texts = [c["text"] for c in corpus]
assert len(texts) == len(set(texts)), "duplicate corpus text"

# ---- queries ------------------------------------------------------------
queries = []
for c in common.CONCEPTS:
    for n, q in enumerate(c["q"], 1):
        text, family, extra, labelled_on, facets = norm(q)
        rel = {c["id"]: 2}
        rel.update(extra)
        for k in rel:
            assert k in known, f"{c['id']}: unknown relevant concept {k}"
        qid = f"q-{c['id']}-{n}"
        queries.append(dict(
            id=qid, query=text, kind="answerable", family=family, topic=c["topic"],
            split=split_of(qid),
            relevant=[{"concept": k, "grade": g} for k, g in sorted(rel.items())],
            facets=c["facets"] if facets is None else facets,
            source="authored against the corpus text",
            labeller=LABELLER, labelled_on=labelled_on))
for n, na in enumerate(common.NO_ANSWER, 1):
    qid = f"na-{n:03d}"
    e = dict(id=qid, query=na["text"], kind="no_answer", family=na["family"], topic=na["topic"],
             split=split_of(qid), relevant=[], facets=[],
             source=na["note"] or "hand-written; no concept in the corpus answers it",
             labeller=LABELLER, labelled_on=LABELLED_ON)
    queries.append(e)
# Real ledger misses the owner labelled (scripts/eval/ledger_misses.py, then
# fold_owner_labels.py). Absent until the owner runs those tools.
owner_q = HERE / "owner_queries.json"
for oq in json.loads(owner_q.read_text(encoding="utf-8")) if owner_q.exists() else []:
    # Keyed by the query's text, so a later fold keeps every id and split.
    qid = "na-ledger-" + hashlib.sha256(oq["query"].encode()).hexdigest()[:10]
    queries.append(dict(id=qid, query=oq["query"], kind="no_answer", family="ledger-miss",
                        topic="ledger", split=split_of(qid), relevant=[], facets=[],
                        source="dogfood ledger recall, labelled no-answer by the owner",
                        labeller=oq["labeller"], labelled_on=oq["labelled_on"]))
qtexts = [q["query"] for q in queries]
assert len(qtexts) == len(set(qtexts)), "duplicate query text"

# ---- pairs --------------------------------------------------------------
pairs, seen = [], set()
KEY_ORDER = ("id", "a", "b", "label", "family", "relation", "concept_types", "split", "source",
             "labeller", "labelled_on", "concepts", "review")


def add_pair(pid, a, b, label, family, source, relation=None, review=None, concepts=None,
             labeller=LABELLER, labelled_on=LABELLED_ON, types=None):
    key = frozenset((a, b))
    if a == b or key in seen:
        return
    seen.add(key)
    e = dict(id=pid, a=a, b=b, label=label, family=family)
    if relation:
        e["relation"] = relation
    if types:
        assert len(types) == 2 and all(t in CONCEPT_TYPES for t in types), (pid, types)
        e["concept_types"] = list(types)
    e.update(split=split_of(pid), source=source, labeller=labeller, labelled_on=labelled_on)
    if concepts:
        e["concepts"] = concepts
    if review:
        e["review"] = review
    pairs.append(e)


for c in common.CONCEPTS:
    src = f"corpus {c['id']} + authored rewording"
    if c["para"]:
        add_pair(f"p-{c['id']}-para", c["text"], c["para"], "paraphrase", "rewording", src, concepts=[c["id"]])
    if c.get("para2"):
        add_pair(f"p-{c['id']}-terse", c["text"], c["para2"], "paraphrase", "terse", src, concepts=[c["id"]])
    for n, r in enumerate(c["rbd"], 1):
        text, family = r[0], r[1]
        relation = r[2] if len(r) > 2 else None
        review = r[3] if len(r) > 3 else None
        a, b = c["text"], text
        # `supersedes` means a is the newer value; `superseded_by` in a
        # module marks a partner newer than its concept, swapped here.
        if relation == "superseded_by":
            a, b, relation = b, a, "supersedes"
        add_pair(f"p-{c['id']}-rbd{n}", a, b, "related_distinct", family,
                 f"corpus {c['id']} + authored contrast", relation=relation, review=review,
                 concepts=[c["id"]])

for n, p in enumerate(common.EXTRA_PAIRS, 1):
    add_pair(p["id"] or f"p-x{n:02d}", p["a"], p["b"], p["label"], p["family"], p["source"],
             relation=p["relation"], review=p["review"], types=p["types"],
             labelled_on=p["labelled_on"] or LABELLED_ON)

# #178's labelled pairs: the dogfood-i3 file/commit-name pairs and the
# synthetic distinct pairs. The 89 synthetic typo duplicates are left out: a
# one-letter typo is not a paraphrase, and they would pull the paraphrase
# quantiles toward 1.0.
SYN = {  # id -> (family, relation, review)
    "syn-neg-000": ("identifier", "contradicts", None), "syn-neg-001": ("identifier", "contradicts", None),
    "syn-neg-002": ("identifier", "contradicts", None), "syn-neg-003": ("identifier", "distinct_entity", None),
    "syn-neg-004": ("identifier", "distinct_entity", None), "syn-neg-005": ("identifier", "contradicts", None),
    "syn-neg-006": ("identifier", "contradicts", None), "syn-neg-007": ("identifier", "contradicts", None),
    "syn-neg-008": ("identifier", "contradicts", None), "syn-neg-009": ("identifier", "contradicts", None),
    "syn-neg-010": ("identifier", "contradicts", None), "syn-neg-011": ("identifier", "contradicts", None),
    "syn-neg-012": ("identifier", "contradicts", None), "syn-neg-013": ("identifier", "contradicts", None),
    "syn-neg-014": ("identifier", "contradicts", None), "syn-neg-015": ("identifier", "contradicts", None),
    "syn-neg-016": ("identifier", "contradicts", None),
    "syn-neg-017": ("identifier", "contradicts", "the two hashes differ in one hex digit; it may be a typo of the same commit (a duplicate), not a second commit"),
    "syn-neg-018": ("identifier", "distinct_entity", None),
    "syn-neg-019": ("identifier", "contradicts", "src/mein.rs reads as a typo of src/main.rs; whether this is a duplicate or a distinct path is the owner's call"),
    "syn-neg-020": ("identifier", "contradicts", None), "syn-neg-021": ("identifier", "distinct_entity", None),
    "syn-neg-022": ("quantity", "contradicts", None), "syn-neg-023": ("quantity", "contradicts", None),
    "syn-neg-024": ("topic-neighbour", None, None), "syn-neg-025": ("topic-neighbour", None, None),
    "syn-neg-026": ("polarity", "contradicts", None), "syn-neg-027": ("topic-neighbour", None, None),
    "syn-neg-028": ("polarity", "contradicts", None), "syn-neg-029": ("polarity", "contradicts", None),
}
L178 = "evidence/issue-178-similar/labelled-pairs.json"
for p in json.loads((ROOT / L178).read_text(encoding="utf-8")):
    if p["source"] == "dogfood-i3" and p["family"] == "file/commit name":
        # File, commit and note names: a writer derives them as Resources,
        # which #198's trigger never considers.
        add_pair(f"p-178-{p['id']}", p["a"], p["b"], "related_distinct", "identifier",
                 f"{L178}#{p['id']} (dogfood store)", relation="distinct_entity",
                 types=("Resource", "Resource"))
    elif p["source"] == "synthetic" and p["label"] == "distinct":
        fam, relation, review = SYN[p["id"]]
        add_pair(f"p-178-{p['id']}", p["a"], p["b"], "related_distinct", fam,
                 f"{L178}#{p['id']} (synthetic)", relation=relation, review=review)

# Unrelated: concept pairs drawn across areas with a fixed seed.
AREA = {"embedding-coverage": "embed", "candle": "embed", "canary": "embed", "calibration": "embed",
        "dogfood-topology": "serve", "multi-client": "serve", "async-ack": "serve",
        "process": "process", "dogfood-findings": "process", "efficacy": "process",
        "performance": "engine", "releases": "engine", "sessions": "engine", "contradictions": "engine",
        "merge-guards": "engine", "canonization": "engine", "g3-graph": "engine", "credentials": "engine"}
for c in corpus:
    assert c["topic"] in AREA, c["topic"]
# Cross-area draws that a reader judged related after all; never drawn.
REJECT = {frozenset(p) for p in [
    ("pro-07", "srv-05"), ("cn-01", "fnd-06"), ("ack-01", "fnd-10"), ("srv-04", "fnd-10"),
    ("fnd-02", "cal-02"), ("srv-06", "pro-02"), ("ack-04", "cnd-01"), ("rel-05", "ack-01"),
    ("can-01", "ack-02"),
]}
AREA_OF = {c["id"]: AREA[c["topic"]] for c in corpus}
AREA_OF.update({"rel-05": "embed", "rel-10": "embed", "cnd-04": "serve"})
# Concepts that touch several areas are kept out of the unrelated draw.
NO_UNRELATED = {"eff-05", "eff-01", "eff-03", "top-02", "emb-06"}
pool = [c for c in corpus if c["id"] not in NO_UNRELATED]
rng = random.Random(190)
n = 0
while n < UNRELATED_PAIRS:
    x, y = rng.sample(pool, 2)
    if AREA_OF[x["id"]] == AREA_OF[y["id"]] or frozenset((x["id"], y["id"])) in REJECT:
        continue
    before = len(pairs)
    add_pair(f"p-u{n + 1:03d}", x["text"], y["text"], "unrelated", "cross-area",
             f"corpus {x['id']} x {y['id']}, drawn across areas (seed 190)", concepts=[x["id"], y["id"]])
    if len(pairs) > before:
        n += 1

# Store-sampled pairs the owner labelled (scripts/eval/sample_store_pairs.py,
# then fold_owner_labels.py). Absent until the owner runs those tools. Folded
# in after every agent pair, so agent ids do not move and an owner label on
# the same two texts as an agent pair relabels that pair in place.
owner_p = HERE / "owner_pairs.json"
# The concept types belong to the two texts, not to the label: a relabel keeps
# them unless the owner's row gives its own.
typed_before = {p["id"] for p in pairs if p.get("concept_types")}
for op in json.loads(owner_p.read_text(encoding="utf-8")) if owner_p.exists() else []:
    key = frozenset((op["a"], op["b"]))
    if key in seen:
        # The owner's label takes precedence over an agent label on the same
        # two texts; the pair keeps its id and says it was relabelled.
        old = next(p for p in pairs if frozenset((p["a"], p["b"])) == key)
        for k in ("relation", "review"):
            old.pop(k, None)
        old.update(a=op["a"], b=op["b"], label=op["label"], family=op["family"],
                   labeller=op["labeller"], labelled_on=op["labelled_on"],
                   source=old["source"] + f"; relabelled by the owner ({op['id']})")
        if op.get("relation"):
            old["relation"] = op["relation"]
        if op.get("concept_types"):
            old["concept_types"] = list(op["concept_types"])
        # Back into add_pair's key order, so a relabel moves no field.
        order = {k: old[k] for k in KEY_ORDER if k in old}
        assert len(order) == len(old), (old["id"], set(old) - set(order))
        old.clear()
        old.update(order)
        continue
    add_pair(f"p-owner-{op['id']}", op["a"], op["b"], op["label"], op["family"],
             "dogfood store copy, cosine-stratified sample", relation=op.get("relation"),
             labeller=op["labeller"], labelled_on=op["labelled_on"], types=op.get("concept_types"))
lost = sorted(i for i in typed_before if not next(p for p in pairs if p["id"] == i).get("concept_types"))
assert not lost, f"an owner relabel dropped concept_types: {lost}"

# #198: the concept type each text of the earlier supersedes pairs would be
# derived as (the anchor concept's corpus type; emb-06 is a re-pin action).
EARLIER_TYPES = {
    "p-emb-01-rbd1": "Observation", "p-emb-03-rbd1": "Logic", "p-emb-06-rbd1": "Resource",
    "p-cnd-06-rbd1": "Observation", "p-cal-03-rbd1": "Constraint", "p-top-01-rbd1": "Constraint",
    "p-srv-04-rbd1": "Logic", "p-rel-04-rbd1": "Constraint", "p-con-01-rbd1": "Constraint",
    "p-con-03-rbd1": "Constraint", "p-cn-01-rbd1": "Constraint", "p-fnd-05-rbd1": "Logic",
}
for p in pairs:
    t = EARLIER_TYPES.get(p["id"])
    if t and "concept_types" not in p:
        # Kept in key order: after relation, before split.
        rest = {k: p.pop(k) for k in list(p) if k not in ("id", "a", "b", "label", "family", "relation")}
        p["concept_types"] = [t, t]
        p.update(rest)
missing = [p["id"] for p in pairs if p.get("relation") == "supersedes" and "concept_types" not in p]
assert not missing, f"supersedes pairs without concept_types: {missing}"


def same_identifiers(a, b):
    """src/graph/canonical.rs same_identifiers, mirrored: fold, then compare
    the digit runs and the identifier-shaped tokens."""
    import re
    import unicodedata

    def fold(s):
        out, gap = [], False
        for ch in unicodedata.normalize("NFC", s):
            if ch in "_-" or ch.isspace():
                gap = True
                continue
            if gap and out:
                out.append(" ")
            gap = False
            out.append(ch.lower())
        return "".join(out)

    def ident(f):
        toks = []
        for t in f.split(" "):
            t = re.sub(r"^[^\w/.]+|[^\w/.]+$", "", t).rstrip(".")
            if t and any((c.isascii() and c.isdigit()) or c in "/." for c in t):
                toks.append(t)
        return toks

    fa, fb = fold(a), fold(b)
    return re.findall(r"[0-9]+", fa) == re.findall(r"[0-9]+", fb) and ident(fa) == ident(fb)


# Every #198 pair is one the #195 identifier guard refuses; otherwise it
# would merge and never reach the trigger.
for p in pairs:
    if p["id"].startswith("p-198-"):
        assert not same_identifiers(p["a"], p["b"]), f"{p['id']}: the identifier guard would not refuse it"

assert len({p["id"] for p in pairs}) == len(pairs)
# #198's pairs stay out of the export: the identifier guard refuses every one
# before any threshold applies, so they barely move the threshold it derives
# (BGE-M3 p99 0.98921 to 0.98934) but lengthen the canary's embed from 62.6 s
# to 87.6 s, past its 60 s timeout.
export = [{"a": p["a"], "b": p["b"]} for p in pairs
          if p["label"] == "related_distinct" and "review" not in p
          and not p["id"].startswith("p-198-")]


def write(name, obj):
    (OUT / name).write_text(json.dumps(obj, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")


write("corpus.json", corpus)
write("queries.json", queries)
write("pairs.json", pairs)
write("related_pairs.json", export)

from collections import Counter  # noqa: E402
print("corpus", len(corpus), Counter(c["origin"] for c in corpus))
print("queries", Counter(q["kind"] for q in queries))
print("pairs", Counter(p["label"] for p in pairs), "review", sum("review" in p for p in pairs))
print("export", len(export))
