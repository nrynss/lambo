"""Authoring helpers for the #190 evaluation set.

Each topic module calls C() once per corpus concept and NA() once per
no-answer query, and P() once per extra pair. build.py imports every t_*.py
module and writes
fixtures/eval/{corpus,queries,pairs,related_pairs}.json.
"""
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
CONCEPTS = []
NO_ANSWER = []
EXTRA_PAIRS = []
_DOGFOOD = json.loads((HERE / "dogfood_texts.json").read_text(encoding="utf-8"))


def D(prefix):
    """A dogfood-store text, resolved by its unique leading prefix."""
    hits = [e for e in _DOGFOOD if e["text"].startswith(prefix)]
    if len(hits) != 1:
        raise SystemExit(f"D({prefix!r}) matched {len(hits)} dogfood texts")
    return hits[0]


def C(id, type, text, src=None, q=(), para=None, rbd=(), facets=(), topic=None):
    """One corpus concept.

    text  authored str, or D(prefix) for a verbatim dogfood-store text
    src   repo path the authored fact comes from (taken from D() otherwise)
    q     answerable queries: str, (str, family) or (str, family, {id: grade});
          this concept is grade 2 unless the dict says otherwise
    para  a rewording that keeps every fact: a paraphrase pair
    rbd   related-but-distinct partners: (text, family[, relation[, review]]);
          relation 'supersedes' when this concept is the newer value,
          'superseded_by' when the partner is (build.py swaps it into a);
          a review note keeps the pair out of the export and the numbers
    facets  short strings a correct answer has to carry (#181)
    """
    if isinstance(text, dict):
        src_list, origin, text = text["sources"], "dogfood", text["text"]
    else:
        src_list, origin = [src], "authored"
    if not src_list[0]:
        raise SystemExit(f"{id}: authored concept needs src")
    CONCEPTS.append(dict(id=id, type=type, text=text, origin=origin,
                         sources=src_list, q=list(q), para=para, rbd=list(rbd),
                         facets=list(facets), topic=topic))


def NA(topic, text, family="near-domain", note=None):
    NO_ANSWER.append(dict(topic=topic, text=text, family=family, note=note))


def P(a, b, label, family, src, relation=None, review=None, types=None, id=None,
      labelled_on=None):
    """A pair outside the concept-anchored ones (e.g. two dogfood texts).

    types        (type of a, type of b): the concept type each text would be
                 derived as (#198); required with relation 'supersedes'
    id           a stable pair id; p-xNN by position otherwise
    labelled_on  the pair's own date; build.py's LABELLED_ON otherwise
    """
    if relation == "supersedes" and not types:
        raise SystemExit(f"P({a[:40]!r}): a supersedes pair needs types")
    EXTRA_PAIRS.append(dict(a=a, b=b, label=label, family=family, source=src,
                            relation=relation, review=review,
                            types=list(types) if types else None, id=id,
                            labelled_on=labelled_on))


def Q(cid, text, family, extra=None, labelled_on="2026-10-27", facets=None):
    """Append one answerable query to an existing concept (#173 follow-up).

    Appended after the concept's own queries, so existing ids and splits
    are unchanged; the query carries its own labelled_on date. `facets`
    overrides the concept's facets when the query asks for a different
    part of the concept than its main fact.
    """
    hits = [c for c in CONCEPTS if c["id"] == cid]
    if len(hits) != 1:
        raise SystemExit(f"Q({cid!r}): no such concept")
    hits[0]["q"].append((text, family, dict(extra or {}), labelled_on,
                         list(facets) if facets is not None else None))
