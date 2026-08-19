#!/usr/bin/env python3
"""G1: record BGE-M3 cosine bands used by recall and hybrid merge.

Requires the local llama.cpp BGE-M3 server used by F:
    python3 evidence/mooshik-g-recall-calibration/measure.py

The strings deliberately use the production no-origin hybrid framing
(``Concept: {content}``) for the merge corpus.  Recall compares a bare query to
that stored framing, just as the F SQLite evidence does.
"""

import json
import math
import urllib.request

URL = "http://127.0.0.1:8080/v1/embeddings"

PAIRS = {
    "true_recall": [
        ("database table for people signing up", "Concept: user schema stores account records"),
        ("login token checking layer", "Concept: auth middleware validates bearer tokens"),
        ("changes that do not break existing clients", "Concept: deployment must stay backward compatible"),
        ("where do we persist customer profiles", "Concept: user schema stores account records"),
        ("verify the API access token", "Concept: auth middleware validates bearer tokens"),
        ("preserve old API consumers during rollout", "Concept: deployment must stay backward compatible"),
    ],
    "paraphrase_merge": [
        ("Concept: register user", "Concept: create account"),
        ("Concept: user schema", "Concept: user data model"),
        ("Concept: delete user", "Concept: remove account"),
        ("Concept: reset password", "Concept: change password"),
        ("Concept: charge card", "Concept: process payment"),
        ("Concept: deploy service", "Concept: ship application"),
        ("Concept: grant access", "Concept: authorize user"),
        ("Concept: sync data", "Concept: reconcile records"),
        ("Concept: make an account", "Concept: register user"),
        ("Concept: roll out the service", "Concept: deploy service"),
    ],
    "related_distinct": [
        ("Concept: user schema", "Concept: user auth"),
        ("Concept: delete user", "Concept: create user"),
        ("Concept: reset password", "Concept: forgot password"),
        ("Concept: charge card", "Concept: credit score"),
        ("Concept: deploy service", "Concept: take down service"),
        ("Concept: grant access", "Concept: revoke access"),
        ("Concept: sync data", "Concept: log data"),
        ("Concept: ship application", "Concept: compile binary"),
        ("Concept: authorize user", "Concept: audit access"),
        ("Concept: database schema", "Concept: database backup"),
    ],
    "unrelated": [
        ("Concept: user schema", "Concept: tropical rainforest"),
        ("Concept: deploy service", "Concept: bake sourdough bread"),
        ("Concept: bearer token validation", "Concept: classical piano recital"),
        ("Concept: charge card", "Concept: mountain weather forecast"),
        ("Concept: retry HTTP request", "Concept: orchid watering schedule"),
        ("Concept: data retention policy", "Concept: bicycle tire pressure"),
        ("Concept: authentication middleware", "Concept: lunar crater mapping"),
        ("Concept: database migration", "Concept: jazz improvisation"),
    ],
}


def embed(text: str) -> list[float]:
    body = json.dumps({"input": text, "model": "bge-m3"}).encode()
    request = urllib.request.Request(URL, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)["data"][0]["embedding"]


def cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    return dot / math.sqrt(sum(x * x for x in a) * sum(y * y for y in b))


for group, pairs in PAIRS.items():
    scores = []
    for left, right in pairs:
        score = cosine(embed(left), embed(right))
        scores.append(score)
        print(f"{group}\t{score:.4f}\t{left}\t<->\t{right}")
    print(
        f"summary\t{group}\tn={len(scores)} min={min(scores):.4f} "
        f"max={max(scores):.4f} mean={sum(scores) / len(scores):.4f}"
    )
