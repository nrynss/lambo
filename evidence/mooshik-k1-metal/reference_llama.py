#!/usr/bin/env python3
"""Reference side of the K1 parity gate: BGE-M3 q8_0 through llama.cpp.

This is the *independent implementation* the candle spike is measured against —
a different tokenizer, a different graph, a different quantization, written by
different people. It is therefore kept deliberately untouched: the vector this
writes is exactly the vector `llama-server` returned, with no re-normalization,
no truncation, no pooling adjustment. Any fix-up applied here would be applied
to both sides of the comparison and would void the gate.

The one thing recorded alongside each vector is its L2 norm, so the readers can
see for themselves whether the reference returns unit vectors rather than having
to take it on trust.

A per-item HTTP failure is recorded as a failure and the sweep continues — that
refusal class (J3-R3-1: HTTP 500 on a large input) is itself a finding.

Usage:
    python3 evidence/mooshik-k1-metal/reference_llama.py \
        --corpus evidence/mooshik-k1-metal/corpus.jsonl \
        --url http://127.0.0.1:8099 \
        --out evidence/mooshik-k1-metal/vectors-llama-q8.jsonl
"""

import argparse
import json
import math
import time
import urllib.error
import urllib.request


def embed(url: str, text: str, timeout: float = 300.0):
    body = json.dumps({"input": text, "model": "bge-m3"}).encode()
    req = urllib.request.Request(
        url.rstrip("/") + "/v1/embeddings",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    started = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as r:
        payload = json.load(r)
    ms = (time.perf_counter() - started) * 1000.0
    return payload["data"][0]["embedding"], ms


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--url", default="http://127.0.0.1:8099")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    items = [json.loads(l) for l in open(args.corpus, encoding="utf-8") if l.strip()]

    ok = 0
    failed = 0
    with open(args.out, "w", encoding="utf-8") as out:
        for item in items:
            try:
                vec, ms = embed(args.url, item["text"])
                norm = math.sqrt(sum(x * x for x in vec))
                out.write(
                    json.dumps(
                        {
                            "id": item["id"],
                            "backend": "llama.cpp-q8_0",
                            "bytes": item["bytes"],
                            "embed_ms": ms,
                            "norm": norm,
                            "vector": vec,
                        }
                    )
                    + "\n"
                )
                ok += 1
            except urllib.error.HTTPError as e:
                detail = e.read().decode("utf-8", "replace")[:400]
                out.write(
                    json.dumps(
                        {
                            "id": item["id"],
                            "backend": "llama.cpp-q8_0",
                            "bytes": item["bytes"],
                            "error": f"HTTP {e.code}",
                            "detail": detail,
                        }
                    )
                    + "\n"
                )
                failed += 1
                print(f"  ! {item['id']} ({item['bytes']} B): HTTP {e.code} {detail[:120]}")
            except Exception as e:  # noqa: BLE001 - a refusal is a datum here
                out.write(
                    json.dumps(
                        {
                            "id": item["id"],
                            "backend": "llama.cpp-q8_0",
                            "bytes": item["bytes"],
                            "error": type(e).__name__,
                            "detail": str(e)[:400],
                        }
                    )
                    + "\n"
                )
                failed += 1
                print(f"  ! {item['id']} ({item['bytes']} B): {type(e).__name__}: {e}")

    print(f"reference: {ok} embedded, {failed} failed -> {args.out}")


if __name__ == "__main__":
    main()
