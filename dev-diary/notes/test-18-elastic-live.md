# #18: manual local Elasticsearch integration checks

Base: main `8b8cbcba27281bbcceb9e0fca244b378265b94f6`.

## Scope and decisions

The four ignored tests in `src/store/tiered/elastic_live.rs` exercise a real
Elasticsearch 8.15.3 server beside a temporary persistent SQLite primary.
They cover index creation/mapping and leased concept projection, fixed-fixture
vector and CLI recall parity, deletes/erase with isolation of another session,
and unavailable-index fallback followed by client reopen, repair and resumed
writes. All data is synthetic and vectors use the fixture embedder. No CI,
production API, dependency, version or workflow change is required.

The module requires `store-sqlite`, `fixtures` and `embed-fixture`; the last
is explicit so valid feature combinations do not import unavailable symbols.
An ordinary run reports ignored; an explicitly requested live test with no
URL fails rather than returning a green skip.

Parity uses the existing test-only primary wrapper with a failing, counted
checked-vector fallback. Both tier and CLI must return the SQLite fixture's
answer with zero fallback calls. This makes an index-query failure visible
instead of letting a SQLite fallback produce a false passing result.

Each test owns a UUID prefix. The assertion body runs in a spawned task;
the surrounding harness deletes explicit owned index names before propagating
an assertion panic. Final container teardown remains required if the cluster
itself is unavailable during cleanup. Outage uses an owned bound loopback
socket that serves no HTTP; the real REST client times out. Restart means
fresh adapter/SQLite handles using durable state, not an ES server restart.

## Running the live Elasticsearch tests

```sh
docker run -d --name lambo-es-live -p 127.0.0.1:59200:9200 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e 'ES_JAVA_OPTS=-Xms512m -Xmx512m' \
  docker.elastic.co/elasticsearch/elasticsearch:8.15.3
curl --fail http://127.0.0.1:59200
XDG_RUNTIME_DIR=$(mktemp -d /tmp/lbX.XXXX) \
  LAMBO_ELASTIC_URL=http://127.0.0.1:59200 \
  cargo test --features recall-elastic,store-sqlite,fixtures,embed-fixture \
  --lib elastic_live -- --ignored --nocapture
docker rm -f lambo-es-live
```

Security is off only because the single-node throwaway container binds to
127.0.0.1 and is removed afterward. Keep the approved image for reuse. Remove
the container after failed runs too. No real credentials or store data are used.

## Review and validation

Preliminary source review found two High, two Medium and one Low finding:
wrong kNN argument order; missing fixture feature gate; wrong projected enum
case; parity that could silently fall back; cleanup skipped after panic.
The applied proposal corrects these with the existing API and test seams.
The first compile also found an unavailable temporary-directory crate; use
the standard library rather than adding a dev-dependency.

The first live run passed three checks and failed CLI parity: the vectorless
fixture concept lacked its required origin `Derives` edge. Adding that edge
fixes the synthetic fixture without changing the number of indexed concepts.
The focused review verified the fix and found no further source issues.

Final live run against local Elasticsearch 8.15.3: **4 passed / 0 failed /
0 ignored**, 1.88 seconds of test execution. The cluster's index listing was
empty afterward; the panic path in the earlier failed run also removed its
owned indices. Ordinary invocation: **0 passed / 0 failed / 4 ignored**.

SQLite all-targets: **2,560 passed / 0 failed / 7 ignored**, unchanged from
baseline (library 2,484/0/5; main 11/0/0; integration binaries 65/0/2;
example 0/0/0). Tier baseline inventory was 86 tests; the suite adds four
explicitly ignored live checks under its feature gate.

Every command below ran locally with
`XDG_RUNTIME_DIR=$(mktemp -d /tmp/lbX.XXXX)`; no CI job was added.

| Gate command | Result |
| --- | --- |
| `cargo fmt --check` | Pass |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | Pass |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | Pass |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | Pass |
| `cargo clippy --all-targets --features store-postgres,store-sqlite,fixtures -- -D warnings` | Pass |
| `cargo clippy --all-targets --features embed-eg2 -- -D warnings` | Pass |
| `cargo clippy --all-targets --features recall-elastic -- -D warnings` | Pass |
| `cargo clippy --all-targets --features embed-candle -- -D warnings` | Pass |
| `cargo test --all-targets --features store-sqlite,fixtures` | Pass, 2,560/0/7 |
| `cargo test --no-run --no-default-features --features store-sqlite` | Pass |
| `cargo check --no-default-features` | Pass |
| `bash scripts/docs/check-mirror-drift.sh` | Pass |

Formatting and ship/recall-elastic clippy were repeated after the final
fixture correction and passed. The Unreleased duplicate-bullet awk check
and `git diff --check` passed. The live module also compiled with
`cargo test --no-run --no-default-features --features
recall-elastic,store-sqlite,fixtures,embed-fixture --lib elastic_live`.
The negative missing-URL check explicitly ran the mapping test with
`-- --ignored --nocapture` and no `LAMBO_ELASTIC_URL`: **0 passed / 1 failed /
0 ignored**, exit 101, with the required missing-variable message. This
expected failure confirms that a manually requested run cannot skip green.
Final focused review is clean with no remaining findings.

## Follow-ups

Hand this local branch to the coordinating session for push and PR; this
work does not push, merge or tag. The separate measurement-only Postgres
runs on the same base had one release-fencing assertion failure in each
row; it is preserved in the private Stream F report rather than changed
by this Elasticsearch test PR. Supersession was not yet merged in that
measured base.
