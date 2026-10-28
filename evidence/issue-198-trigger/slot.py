"""#198 call 2: does the position of the changed identifier separate supersession
from distinct facts?

The measured reference for the Rust trigger (src/graph/suspected/slot.rs).
Committed as run, except that the inputs default to this repository:

    python3 -I evidence/issue-198-trigger/slot.py [--with-reports] [<repo> [<cosines.json> [<regime_dir>]]]

<cosines.json> is the eval_measure per-pair detail (or the committed
cosines-bge-m3-self.json extract of it); only `framing == "self"` rows are
read. --with-reports adds the verb "reports" to CUES, the one change the
Rust lexicon makes (measured on dev). slot.txt is the output without it."""
import collections, difflib, json, os, re, sys

_here = os.path.dirname(os.path.abspath(__file__))
_args = [a for a in sys.argv[1:] if a != "--with-reports"]
WITH_REPORTS = "--with-reports" in sys.argv[1:]
repo = _args[0] if len(_args) > 0 else os.path.normpath(os.path.join(_here, "..", ".."))
detail_path = _args[1] if len(_args) > 1 else os.path.join(_here, "cosines-bge-m3-self.json")
regime_dir = _args[2] if len(_args) > 2 else _here
# Reuse the reviewer's same()/jac() so the baseline matches the earlier numbers.
exec(open(regime_dir + "/regime.py").read().split("P=json.load")[0])

P = {p["id"]: p for p in json.load(open(repo + "/fixtures/eval/pairs.json"))}
D = json.load(open(detail_path))
COS = {x["id"]: x["cosine"] for x in D["pairs"] if x["framing"] == "self"}
TH = 0.85

# Words after which a sentence states its value: copulas, common predicate
# verbs and the separators that introduce a value.
CUES = set("""is are was were be been becomes became equals = : runs ran serves served
uses used pins pinned defaults default takes took holds held has have had sets set
reads returns gives gave scores scored applies applied acknowledged measured caps
capped listens listened binds bound points pointed lands landed ships shipped stays
now at to in of""".split())
if WITH_REPORTS:
    CUES.add("reports")

def toks(s):
    return [t for t in re.split(r"\s+", s.strip()) if t]

def norm(t):
    return t.strip(".,;()'\"").lower()

def has_id(t):
    return bool(re.search(r"\d", t)) or bool(re.search(r"[A-Z].*[A-Z]|_|-|/|\.", t.strip(".,;")))

def spans(a, b):
    sm = difflib.SequenceMatcher(a=[norm(t) for t in a], b=[norm(t) for t in b], autojunk=False)
    return [(i1, i2, j1, j2) for op, i1, i2, j1, j2 in sm.get_opcodes() if op != "equal"]

def first_cue(ts):
    for i, t in enumerate(ts):
        n = norm(t)
        if n in CUES or t.endswith(":"):
            return i
    return None

def features(p):
    a, b = toks(p["a"]), toks(p["b"])
    sp = spans(a, b)
    one = len(sp) == 1
    ca, cb = first_cue(a), first_cue(b)
    # Value slot: every differing span starts after the first cue in both texts.
    value = bool(sp) and ca is not None and cb is not None and all(i1 > ca and j1 > cb for i1, _, j1, _ in sp)
    return {"one_span": one, "value_slot": value, "n_spans": len(sp)}

rows = collections.defaultdict(list)
for pid, p in P.items():
    ct = p.get("concept_types") or []
    if not (len(ct) == 2 and ct[0] == ct[1] and ct[0] in ("Logic", "Constraint", "Observation")):
        continue
    if "review" in p or same(p["a"], p["b"]) or pid not in COS:
        continue
    k = p.get("relation") or p["family"]
    if k not in ("supersedes", "distinct_entity", "same-event"):
        continue
    if COS[pid] < TH:
        continue
    f = features(p)
    f.update(id=pid, kind=k, split=p["split"], j=jac(p["a"], p["b"]))
    rows[p["split"]].append(f)

RULES = {
    "baseline J>=0.9": lambda f: f["j"] >= 0.9,
    "one span": lambda f: f["one_span"],
    "value slot": lambda f: f["value_slot"],
    "one span + value slot": lambda f: f["one_span"] and f["value_slot"],
    "one span + value slot + J>=0.9": lambda f: f["one_span"] and f["value_slot"] and f["j"] >= 0.9,
}

def wilson_lo(k, n, z=1.96):
    if n == 0:
        return float("nan")
    ph = k / n
    return (ph + z * z / (2 * n) - z * ((ph * (1 - ph) + z * z / (4 * n)) / n) ** 0.5) / (1 + z * z / n)

pos_total = {s: sum(1 for f in rows[s] if f["kind"] == "supersedes") for s in rows}
print(f"in-regime pairs at cosine >= {TH}:", {s: collections.Counter(f["kind"] for f in rows[s]) for s in rows})
for name, rule in RULES.items():
    print(f"\n== {name}")
    for s in ("dev", "held_out"):
        hit = [f for f in rows[s] if rule(f)]
        tp = sum(f["kind"] == "supersedes" for f in hit)
        fp = len(hit) - tp
        prec = tp / len(hit) if hit else float("nan")
        print(f"  {s:8} TP {tp:2} FP {fp:2} precision {prec:.2f} (95% low {wilson_lo(tp, len(hit)):.2f}) recall {tp}/{pos_total[s]}")
    if name == "one span + value slot":
        for s in ("dev", "held_out"):
            fps = [f["id"] for f in rows[s] if rule(f) and f["kind"] != "supersedes"]
            fns = [f["id"] for f in rows[s] if not rule(f) and f["kind"] == "supersedes"]
            print(f"  {s} false positives: {fps}")
            print(f"  {s} missed supersedes: {fns}")
