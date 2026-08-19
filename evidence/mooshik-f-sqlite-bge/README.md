# SQLite vector leg under a real embedder — semantic recall, captured

Closes the evidence half of **F-R1-5**. The adversarial review's finding was not that
the vector leg is broken — it demonstrably fires — but that the docs promised recall
*by meaning* on SQLite while every committed artifact used `FixtureEmbedder`, which is
deterministic rather than semantic. No test, runbook, or evidence file showed the
SQLite vector leg producing a semantic hit under any shipped embedder.

This is that run: a real `SqliteStore`, a real `kind = "bge_m3"` embedder, and recall
queries sharing **no vocabulary** with the concepts they retrieve.

- **Binary:** `target/debug/lambo` 0.2.2, built `--features store-sqlite,embed-bge`
- **Embedder:** BGE-M3, 1024-d, served by a local llama.cpp `llama-server` on
  `http://127.0.0.1:8080` (`GET /health` → `{"status":"ok"}`; OpenAI-compatible
  `POST /v1/embeddings`, which is exactly the route the `bge_m3` adapter speaks)
- **Store:** SQLite, `f-bge.db`, provisioned fresh at the start of the run
- **Session:** `f-bge-semantic`, three concepts derived through `lambo derive`

## Artifacts

| Artifact | What it is |
|---|---|
| `transcript.txt` | The whole run, start to finish: version and health probes, the config, `provision`, three `derive` calls, the durable readback, three `recall` calls with their shared-vocabulary checks, the raw-cosine probe, and the width-pin refusal. |
| `run.sh` | The driver that produced it. Committed so the run is reproducible rather than described. |
| `cosine_probe.py` | Recomputes cosine between each query and the **durable** stored vectors, outside Lambo. |
| `lambo.bge-sqlite.toml` | The exact config the run used. |
| `f-bge.db` | The resulting SQLite store — the durable truth the readback and the probe query. |

## What the run shows

**The vectors are real and durable.** The session stamped its contract and all three
concepts carry full-width vectors:

```
sessions: f-bge-semantic | kind=bge_m3 | model=NULL | dim=1024
auth middleware validates bearer tokens | blob_bytes=12786 | elements=1024
deployment must stay backward compatible | blob_bytes=12786 | elements=1024
user schema stores account records | blob_bytes=12781 | elements=1024
```

**No query shares a word with any concept.** The transcript prints the check per
query rather than asserting it in prose — every line reads `NONE`. So the keyword leg
can contribute nothing, and any ranking signal has to come from the vector leg.

**Two of the three queries produce a genuine semantic hit.** Cosine against the
durable vectors, from `cosine_probe.py`:

| Query | Top concept by cosine | Cosine | Margin over 2nd | Surfaces in `recall`? |
|---|---|---|---|---|
| `database table for people signing up` | `user schema stores account records` | **0.5706** | +0.1586 | **Yes** — ranked first, score 0.29 against a 0.25 floor |
| `login token checking layer` | `auth middleware validates bearer tokens` | **0.5823** | +0.1652 | **Yes** — ranked first, score 0.49 against a 0.25 floor |
| `changes that do not break existing clients` | `deployment must stay backward compatible` | 0.3991 | +0.0476 | **No** — see below |

The first two are the artifact the review asked for: a query with no shared
vocabulary recalling the right derived concept, on SQLite, under a shipped embedder.

## The third query is an honest miss, and the reason is worth recording

Its cosine ranking is *correct* — `deployment must stay backward compatible` does come
first — but at 0.3991 with a thin +0.0476 margin. Recall merges the legs by taking the
**maximum** of each concept's leg scores against the flat `RECENT_SCORE = 0.5` that
every recent-interaction concept receives (`src/recall/candidates.rs`). A vector score
below 0.5 is therefore invisible: `max(0.5, 0.399) == 0.5` for the right concept and
`0.5` for the other two, so all three tie and the vector leg changes nothing. That is
exactly what the transcript shows — three concepts at the flat 0.25 displayed floor.

So the threshold that decides whether a semantic hit *surfaces* is the recent-leg
floor, not a vector-leg threshold. Two of these three queries clear it; one does not.
Recording the one that does not is the point — a capture that only kept the wins would
be the same kind of overclaim the finding was about.

(The displayed recall scores are not the cosines: they pass through the leg merge and
per-concept-type scaling, which is why 0.5706 shows as 0.29 for an `Entity` and 0.5823
shows as 0.49 for a `Logic`. `cosine_probe.py` exists so the raw number is on the
record independently.)

## The context-framing asymmetry, visible in the numbers

`hybrid::derive` embeds the concept **with its context** — `"{content} — {origin}"`,
or `"Concept: {content}"` when there is no origin text, which is the CLI path here —
while `recall` embeds the query **bare**. Every cosine above is therefore a bare query
against a *framed* string, and the framing costs some similarity. With BGE-M3 the
ranking survives it, which is the whole point; with `FixtureEmbedder` it is the
difference between a hit and a miss, which is why the in-tree acceptance test needs a
test-only `ContextTolerantEmbedder` that strips the framing before delegating. No such
wrapper exists in production. This asymmetry is now documented outside the test
comment: `graph/hybrid.rs`'s `context_text` doc, the CLI reference, and the
End-to-end page.

## Bonus: the width pin refuses a real disagreement (F-R1-2)

Step 6 re-runs the same `recall` against the same database with one character changed
— `[embedder] dim` 1024 → 768, against `store.vector_dim = 1024` — and it is refused
at process resolution:

```
lambo recall: failed to build backends: config: store.vector_dim is pinned to 1024 but
the configured embedder emits 768 — refusing to resolve: the pin asserts what this
database already holds, so serving with a different width would write vectors no reader
can interpret (drop the pin, change the embedder, or re-embed the database)
```

Before this remediation that disagreement was **unreachable**: SQLite's
`vector_dimensions()` echoed the embedder's own width, so `check_vector_compatibility`
compared a number to itself and could not fail on any path reaching it.

## Reproducing

```sh
# llama-server with BGE-M3 weights on :8080, then:
cargo build --features store-sqlite,embed-bge
./evidence/mooshik-f-sqlite-bge/run.sh 2>&1 \
  | tee evidence/mooshik-f-sqlite-bge/transcript.txt
```

The embedding server is not deterministic across builds and quantizations, so the
cosines are expected to move in the third decimal place. The orderings and the
above/below-0.5 verdicts are the claims.
