#!/usr/bin/env python3
"""Build the K1 parity corpus: the committed evidence texts plus a multilingual
size spread from 32 B to 8 KiB.

Deterministic — no randomness, no clock — so a re-run produces a byte-identical
`corpus.jsonl` and the parity numbers stay comparable across runs.

The evidence half is the exact text the committed BGE-M3 capture
(`evidence/mooshik-f-sqlite-bge/`) embedded: the three derived concepts, the
same three concepts under the `hybrid::derive` context framing, and the three
recall queries the cosine probe uses.

Usage:  python3 evidence/mooshik-k1-metal/corpus.py > evidence/mooshik-k1-metal/corpus.jsonl
"""

import json
import sys

# --- The committed evidence corpus (evidence/mooshik-f-sqlite-bge/) ----------

CONCEPTS = [
    "auth middleware validates bearer tokens",
    "deployment must stay backward compatible",
    "user schema stores account records",
]

# `hybrid::derive` embeds a concept with its context; the CLI path with no
# origin text produces exactly this framing (see the F evidence README).
FRAMED = [f"Concept: {c}" for c in CONCEPTS]

QUERIES = [
    "database table for people signing up",
    "login token checking layer",
    "changes that do not break existing clients",
]

# --- The synthetic multilingual spread --------------------------------------

# One seed sentence per script. Repeated and cut at a character boundary to hit
# each target byte size, so every language is exercised at every size.
SEEDS = {
    "en": "The write queue admits an interaction only when the lane has room for it. ",
    "de": "Die Schreibwarteschlange nimmt eine Interaktion nur auf, wenn die Spur Platz hat. ",
    "es": "La cola de escritura admite una interacción sólo cuando el carril tiene espacio. ",
    "ru": "Очередь записи принимает взаимодействие только тогда, когда в полосе есть место. ",
    "zh": "只有当通道还有余量时，写入队列才会接受一次交互记录。",
    "ja": "レーンに空きがある場合にのみ、書き込みキューは対話を受け付けます。",
    "ko": "레인에 여유가 있을 때에만 쓰기 큐가 상호작용을 받아들입니다. ",
    "ar": "لا يقبل طابور الكتابة أي تفاعل إلا عندما يتوفر متسع في المسار. ",
    "hi": "लेखन कतार किसी अंतःक्रिया को तभी स्वीकार करती है जब लेन में जगह हो। ",
    "he": "תור הכתיבה מקבל אינטראקציה רק כאשר יש מקום בנתיב. ",
}

# 32 B is the floor the spec names; 8 KiB the ceiling (BGE-M3's capacity is
# 8192 *tokens*, so 8 KiB of any script stays inside it).
SIZES = [32, 128, 512, 1024, 2048, 4096, 8192]


def sized(seed: str, target: int) -> str:
    """Repeat `seed` and cut at a character boundary, <= target bytes."""
    out = ""
    while len(out.encode("utf-8")) < target:
        out += seed
    # Trim back to the largest prefix that fits.
    while len(out.encode("utf-8")) > target:
        out = out[:-1]
    return out


def main() -> None:
    items = []

    for i, text in enumerate(CONCEPTS):
        items.append({"id": f"evidence-concept-{i}", "group": "evidence", "text": text})
    for i, text in enumerate(FRAMED):
        items.append({"id": f"evidence-framed-{i}", "group": "evidence", "text": text})
    for i, text in enumerate(QUERIES):
        items.append({"id": f"evidence-query-{i}", "group": "evidence", "text": text})

    for lang, seed in SEEDS.items():
        for size in SIZES:
            text = sized(seed, size)
            items.append(
                {
                    "id": f"synth-{lang}-{size}",
                    "group": "synthetic",
                    "lang": lang,
                    "target_bytes": size,
                    "text": text,
                }
            )

    # A few shapes that are not prose, because an embedder in a dev-memory tool
    # sees these constantly.
    items.append(
        {
            "id": "edge-code",
            "group": "edge",
            "text": "fn resolve_backends(cfg: &Config) -> Result<Backends> {\n"
            "    let dim = cfg.embedder.dim;\n"
            "    if let Some(pin) = cfg.store.vector_dim {\n"
            "        if pin != dim { bail!(\"width pin {pin} != embedder {dim}\"); }\n"
            "    }\n"
            "    Ok(Backends::new(dim))\n}",
        }
    )
    items.append(
        {
            "id": "edge-mixed-script",
            "group": "edge",
            "text": "The lease holder は writer で、proxies forward bytes — لا يحمل رسمًا بيانيًا.",
        }
    )
    items.append(
        {
            "id": "edge-punctuation",
            "group": "edge",
            "text": "!!! ??? ... --- >>> <<< ### @@@ $$$ %%% ^^^ &&& *** ((( ))) [[[ ]]] {{{ }}}",
        }
    )

    for item in items:
        item["bytes"] = len(item["text"].encode("utf-8"))
        sys.stdout.write(json.dumps(item, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
