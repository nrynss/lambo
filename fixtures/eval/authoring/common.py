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


def P(a, b, label, family, src, relation=None, review=None):
    """A pair outside the concept-anchored ones (e.g. two dogfood texts)."""
    EXTRA_PAIRS.append(dict(a=a, b=b, label=label, family=family, source=src,
                            relation=relation, review=review))
