# Changelog

## 0.4.0 (2026-10-11)

### Highlights

- **Images in memory (#22).** A new `embeddinggemma2` embedder (feature
  `embed-eg2`, carried by the released binaries) runs EmbeddingGemma 2 on
  llama.cpp's `llama-server`. It embeds text, and images too when the server
  is started with `--mmproj`, into one space. `lambo_derive_image` and
  `lambo derive-image` store an image concept, and `lambo_recall` and
  `lambo recall` search by an image or a client query vector. Every image is
  sent in one canonical form, a lossless PNG whose longer side is 768 px, so
  an image embeds nearly the same at any submitted size (the limits are
  under Added).
- **Multi-session `lambo serve` (#32).** One HTTP serve holds several
  sessions at `/mcp/s/{session}`. Each `[[serve.credential]]` token reaches
  only its own sessions. Sessions in a credential's reach attach on demand
  within the `[serve]` bounds. An operator can erase a session or list
  sessions over HTTP. A stdio serve takes its session from
  `[[serve.projects]]`.
- **Session erasure (#23).** `lambo erase-session` erases a whole session in
  one transaction on every store and leaves a tombstone that no writer can
  take over.
- **A multi-session read-only portal (#4, #92).** `lambo serve-web` serves an
  allowlist of sessions. `[[web.credential]]` gives each token its own read
  scope, and the page offers a session picker. Without a credential it checks
  `Host` against DNS rebinding. It also sends `nosniff`, and its page carries
  a strict Content-Security-Policy.
- **Fresh and young memory ranks fairly (#79, #95).** A concept the daemon
  has not scored yet now ranks by query relevance instead of below older
  noise. In a session younger than 10 minutes, recency uses a 10-minute
  floor, so a scheduler stall can no longer flip two scores.
- **A 4 MiB request cap on every transport (#101).** Stdio and the session
  endpoint now refuse a request over 4 MiB before parsing it, as HTTP
  already did.
- **Merges stay fresh on PostgreSQL and CockroachDB (#60).** A paraphrase
  derived seconds after the original now merges into it while the flush
  lags, instead of becoming a permanent near-duplicate.
- **An optional Elasticsearch recall tier (#18).** `[recall] kind =
  "elastic"` (feature `recall-elastic`, carried by the released binaries)
  mirrors each flush to an index that serves recall's vector leg. The store
  stays the source of truth.
- **Faster recall on SQLite (#8, #14).** The session holder ranks vectors
  from memory: warm recall p50 went from 219 ms to 4.3 ms on a
  3,600-concept session. A repeated query reuses its cached vector.

### Upgrading from 0.3.0

1. **Re-provision every store.** Run `lambo provision` before this build
   attaches. Stores gain a `concepts.embedding_source` column (#22), and the
   preflight refuses an unprovisioned store by name. Provisioning only adds
   the column (see Breaking).
2. **Building from source needs Rust 1.99 or newer.** The crate also moves to
   edition 2024 (#42).
3. **Update library code.** Struct literals and exhaustive matches break;
   `..Default::default()` or a wildcard arm fixes most of them. The details
   are under Breaking.
   - `RecallParams`: `image`, `query_vector`; it now derives `Default`.
   - `Concept`: `embedding_source`.
   - `Config` and `EmbedderConfig`: `accept_client_vectors`.
     `EmbedderConfig`: `images`.
   - New enum variants: `EmbedderKind::EmbeddingGemma2`,
     `WriteKind::DeriveImage`, `WriteIntentPayload::DeriveImage`,
     `ReplayBlockReason::ImageConfig`, `LamboError::ImageIdTaken` and
     `EmbedError::Unsupported`. `EmbedError` is now `#[non_exhaustive]`.
   - `LamboFile`: `serve`. `ServeOptions`: `sessions`, `bounds` and
     `credentials` (`ServeOptions::new` fills all three).
   - `lambo::cli::serve_web::Args`: `sessions`, `allowed_hosts` and
     `credentials`. `lambo::config::WebConfig`: `list_sessions`,
     `inherit_serve_credentials` and `credentials`.
   - `Embedder` and `GraphStore` gain methods with defaults, so existing
     adapters compile unchanged (see Added). Two caveats:
     - A wrapping embedder should forward `embed_query`, `modalities` and
       `embed_image`.
     - `GraphStore::erase_session` refuses by default.
4. **New `lambo.toml` keys.** All of them are optional, and 0.3.0 refuses a
   file that has `[serve]` or `[web]`. See Added and the configuration
   reference.
   - `[serve]`: `sessions`, `default_session`, `max_attached`,
     `attach_concurrency`, `idle_detach_secs` and `per_session_rps`.
   - `[[serve.projects]]`.
   - `[[serve.credential]]`: `name`, `token_env`, `sessions`,
     `session_prefix`, `create`, `erase` and `admin`.
   - `[web]`: `sessions`, `allowed_hosts`, `view_ttl_ms`,
     `max_loaded_sessions`, `load_concurrency`, `recall_concurrency`,
     `list_sessions` and `inherit_serve_credentials`.
   - `[[web.credential]]`: `name`, `token_env`, `sessions` and
     `session_prefix`.
   - `[embedder]`: `kind = "embeddinggemma2"` with `url`, `dim`, `model` and
     `images`.
   - `[embedder] accept_client_vectors`, also set by
     `LAMBO_ACCEPT_CLIENT_VECTORS`.
   - `[embedder] api_key_env` for `bge_m3` and its `openai` alias (#21).
   - `[recall]`: `kind = "elastic"`, `url`, `api_key = { env = ... }`,
     `index_prefix`, `refresh` and `timeout_ms`.
5. **Check these behaviour changes.**
   - **Portal `Host` check.** `lambo serve-web` with no credential now
     answers `403` to any `Host` other than `localhost`, `127.0.0.1` or
     `[::1]`. A reverse proxy that forwards its public name as `Host`
     (Caddy's default) must pass that name with `--allowed-host` or
     `[web] allowed_hosts`. An exhibit launched by
     `scripts/aws-infra/launch_exhibit_ec2.py` before this release needs the
     user-data edit given under Breaking, or a relaunch.
   - **Portal security headers.** Every routed response carries
     `X-Content-Type-Options: nosniff`. The HTML page also carries a
     Content-Security-Policy that allows no inline script or style, so a
     proxy that injects either into the page is blocked.
   - **4 MiB cap on stdio.** A stdio request over 4 MiB gets JSON-RPC error
     `-32600` instead of reaching the tool. Split a large `lambo_derive`
     batch into smaller calls.
   - **HTTP `lambo serve` routes and auth.**
     - It serves exactly `/mcp` and `/mcp/s/{session}`, so drop a trailing
       slash from `/mcp/`.
     - Once any `[[serve.credential]]` is configured, loopback requests need
       a token too.
     - `--rate-limit-rps` applies per credential.
   - **Recency in young sessions (#95).** Recency scores change in sessions
     younger than 10 minutes. Sessions of 10 minutes or more score bit for
     bit as before.
   - **Cold mode (#79).** While a relevant concept is still unscored, that
     recall ranks every hit by `w_query` × query relevance. Recall lines in
     `serve --ledger` carry `cold_start`.
   - **Receipts and error text.** A hybrid `record_action` receipt reads
     `(M embedded)`. `lambo.toml` errors no longer quote values, and a parse
     error reads `(line N, column M)`.
   - **Shutdown watchdog (#40).** Set a supervisor kill timeout above 20 s;
     30 s is recommended.
6. **EmbeddingGemma 2 uses the `lambo-eg2-v2` profile.** Its contract model
   defaults to
   `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2`. Use
   `llama-server` b11452 or later, with `--mmproj` for images, and these
   flags: `--image-min-tokens 280 --image-max-tokens 280 --batch-size 8192
   --ubatch-size 8192`. The full command line is in `lambo.example.toml`.
   `lambo-eg2-v1` was never released, so there is nothing to migrate.
7. **One-way changes.**
   - Going back to 0.3.0 is loud by design: 0.3.0 cannot read an unapplied
     image write intent, so it fails to load that session.
   - `lambo re-embed` over image concepts needs `--drop-image-vectors`.
     That nulls their vectors until each image is derived again.
   - Erasing a session is permanent. Reusing an erased id takes a
     deliberate operator UPDATE.
   - Never delete a lease row. A release now keeps the row and its fencing
     token, and the operator override is an UPDATE.
   - Existing canonical keys are not rewritten (#25).

### Known issues

- Recall scores and the merge threshold are not calibrated per embedder
  ([#87](https://github.com/nrynss/lambo/issues/87)). They were tuned on
  BGE-M3. Under EmbeddingGemma 2, irrelevant vector hits score high and
  outrank the recent leg, and most paraphrases stay unmerged.
- Hybrid derive clones the whole session graph under the write lock on every
  commit ([#102](https://github.com/nrynss/lambo/issues/102)). The clone
  took about 10.7 ms at 10k concepts, 60 ms at 50k and 168 ms at 100k, and
  each recall and derive on the session waits for it.
- Some live database tests run only by hand. CI's `postgres-live` job runs a
  fixed set, which does not yet include #60's live merge-freshness test.
  The `cockroach-live` job is disabled.
- There is no Windows binary
  ([#39](https://github.com/nrynss/lambo/issues/39)). v0.2.2 is the last
  release with one.

### Breaking

- `RecallParams` (the `lambo_recall` parameters, re-exported from
  `lambo::mcp::server`) gains two public fields, `image: Option<WireImage>`
  and `query_vector: Option<WireQueryVector>` (#22 PR 6), so code that
  builds it with a struct literal must add `image: None, query_vector:
  None`, or end the literal with `..Default::default()`: `RecallParams`
  now derives `Default`, so the next optional field will not break such a
  literal again. Deserializing it, and its published schema, are
  unaffected.
- `Concept` gains a public field, `embedding_source: Option<EmbeddingSource>`
  (#22). Code that builds a `Concept` with a struct literal must add
  `embedding_source: None`; code that reads or deserializes one is
  unaffected (the field is serde-defaulted and omitted when `None`, so
  fixture JSON and every output that prints a concept are unchanged).
- Stores gain a nullable `concepts.embedding_source` column (#22). An
  existing SQLite, Postgres or Cockroach store must be re-provisioned with
  `lambo provision` before this build attaches: the column preflight refuses
  it by name until then, as it did for `human_confirmed`. Provisioning only
  adds the column; existing rows read `NULL`, meaning embedded from their
  own content.
- `WriteKind` and `WriteIntentPayload` each gain a `DeriveImage` variant
  (#22), so library code that matches either exhaustively needs the new arm.
  An unapplied image intent is unreadable to an older build, which fails to
  load the session rather than replay it without its vector (design risk R5:
  a downgrade is loud). A settled image intent (applied or failed) keeps
  only its outcome: the store overwrites its payload with an empty text
  derive, so its vector does not outlive it and an older build loads it.
  `Graph::reembed_all` now leaves
  image concepts out of its coverage rule and refuses while one carries a
  vector; `Graph::reembed_all_dropping_image_vectors` is the variant that
  nulls them.
- `ReplayBlockReason` gains a variant, `ImageConfig` (#22), and the
  `write_queue_replay_blocked` stat a value, `"image_config"`: a durable
  image intent replayed by a process whose strategy is not `hybrid` or
  whose store has no vector search stops the replay with that reason and a
  log line saying what to change, rather than the generic `"other"`.
- `EmbedError` gains a variant, `Unsupported` (#22): an embedder refusing a
  kind of input it cannot embed at all, such as an image sent to a text-only
  model. `is_transient()` classes it as permanent. `EmbedError` is also now
  `#[non_exhaustive]`, so library code that matches it outside this crate
  needs a wildcard arm (or can call `is_transient()`), and later variants
  will not break it again. Code that implements `Embedder` is unaffected (see
  Added). The new `ImageMime` enum is `#[non_exhaustive]` from the start.
- `LamboFile` gains a public `serve: ServeConfig` field (#32). Code that
  builds a `LamboFile` with a struct literal must add
  `serve: Default::default()`; code that parses one is unaffected.
- `ServeOptions` gains a public `sessions: Vec<String>` field (#32, fourth
  part): every session the serve pins, with `session` the default among them.
  Code that builds `ServeOptions` with a struct literal must add it
  (`sessions: vec![session.clone()]` keeps one session);
  `ServeOptions::new` fills it. `lambo serve --session` is now a repeatable
  flag.
- `ServeOptions` gains a public `bounds: SessionBounds` field (#32, sixth
  part): the `[serve]` bounds on attached sessions. Code that builds
  `ServeOptions` with a struct literal must add `bounds:
  SessionBounds::default()` (or `SessionBounds::from_config(&file.serve)`);
  `ServeOptions::new` fills the defaults.
- An HTTP `lambo serve` with a credential that reaches past its pinned
  sessions (a `session_prefix`, or an unpinned name) now attaches those
  sessions on demand (#32, sixth part), where before such a request got
  the `404`. Such a serve, even with one pinned session, no longer runs the
  startup election and does not exit on a lost lease: only the session that
  lost it is detached. A serve whose credentials reach only the pinned
  sessions (the legacy token, the implicit `local`, or exact pinned names)
  is unchanged.
- `lambo serve --transport http` serves exactly `/mcp` and
  `/mcp/s/{session}` (#32). A request to any other path under `/mcp/`
  (which reached the one session before, because the service ignored the
  path) gets a plain 404 now. That includes `/mcp/` with a trailing slash:
  a client configured with `http://host:port/mcp/` must drop the slash.
- `lambo serve --transport http` enforces `[[serve.credential]]` (#32,
  fifth part). A loopback serve whose `lambo.toml` configures any credential
  no longer accepts a request without a token: every request must present
  one of the configured tokens (or `LAMBO_AUTH_TOKEN` / `--auth-token`) and
  reaches only that credential's sessions. A loopback serve with no
  credential and no token is unchanged. `ServeOptions` gains a public
  `credentials: Vec<ServeCredential>` field; code that builds `ServeOptions`
  with a struct literal must add `credentials: Vec::new()` (`ServeOptions::new`
  fills it).
- With several credentials, `--rate-limit-rps` applies per credential and
  each credential may hold at most `--max-sessions` divided by the number of
  credentials (rounded down, at least 1) MCP sessions (#32, fifth part). A
  serve with one credential (a legacy token alone, or none on loopback)
  keeps its old limits. A configured or legacy token with surrounding
  whitespace, a byte outside printable ASCII or more than 4096 bytes now
  refuses the start (exit 2); a request with two `Authorization` headers
  gets `401`. An `initialize` counts against the cap and the share whenever
  it would mint an MCP session: one carrying `Last-Event-ID`, or an
  `Mcp-Session-Id` that is not visible ASCII, no longer slips past them, and
  initializes arriving together can no longer overshoot either. The MCP
  session is attributed to its credential even if the client disconnects
  before the response, and a client that disconnects from a sessionless
  request (a per-request-protocol call) cancels it, as rmcp does. The body
  of a session-opening request must arrive in full within 30 s (`408`
  otherwise), and one still arriving holds no slot of the session cap; a
  chunked one is held to the same 4 MiB as a declared one. Other requests'
  bodies are not read before routing. Only an `initialize` counts against
  the cap while in flight, so parallel sessionless calls (per-request
  `tools/call`, `server/discover`) are no longer refused at a credential's
  share.
- A `LAMBO_AUTH_TOKEN` that is set but not valid UTF-8 refuses the start of
  `lambo serve --transport http` and `lambo serve-web` (exit 2, naming the
  variable) instead of being read as unset. A stdio `lambo serve` ignores
  `LAMBO_AUTH_TOKEN` and `--auth-token` again whatever their value: since the
  stricter checks above, a stray token they refuse had made it exit 2.
- Minimum supported Rust is now 1.99 (`rust-version = "1.99"` in
  `Cargo.toml`; there was none before), and the crate moves from edition 2021
  to edition 2024. The pinned toolchain moves from 1.97.1 to 1.99.0, and CI and
  the release workflow install it directly. Building from source, including
  `cargo install lambo`, needs Rust 1.99 or newer (#42). The edition change
  alters no behaviour: every drop-order site the migration lints flagged was
  reviewed and none moves a lock release, and formatting stays on the 2021
  style edition for now.
- Canonical keys change for text containing some invisible codepoints (#25,
  re-landing review finding V1, which was fixed on 2026-08-15 and then lost
  the same day). Stored concepts keep the keys they were written with; nothing
  is re-keyed on load, so every existing store still loads and its uniqueness
  check cannot newly collide.
  - The Mongolian free variation selectors `U+180B`-`U+180D` and `U+180F` are
    still accepted in content but are now stripped from canonical keys. A
    concept already stored with one keeps its old key, so a new derive of the
    same text no longer matches it. That derive creates a new concept, or merges
    into an existing plain-text concept with the stripped key, and the old row
    is left out of future matching.
  - Text containing the unassigned `Default_Ignorable_Code_Point` codepoints
    `U+2065`, `U+FFF0`-`U+FFF8`, `U+E0080`-`U+E00FF` and `U+E01F0`-`U+E0FFF` is
    now refused wherever a surface size-checks text: concept and action
    content, inspect focus, recall query, and agent and session ids.
    Stored content holding them still loads and renders, but the same text can
    no longer be submitted again.
  - **Operator action:** none expected. The lambo dogfood store (3,966 concepts)
    held none of these codepoints in content or keys when checked on
    2026-10-08. A store that does hold them can be found by comparing each
    stored `canonical_key` with one recomputed from its content.
- Under the hybrid strategy (the default) an applied `record_action` receipt
  now reads `recorded action: N concept(s) created (M embedded), E edge(s)`
  and its JSON carries `embedded`, as a hybrid `derive` receipt does. Under
  `canonical` the sentence and JSON are unchanged. A client that parses the
  sentence must accept the optional `(M embedded)`;
  `scripts/loadtest/check_durability.py` now does.
- Hybrid `derive` refuses more inputs, each with a `Config` error and nothing
  written, because `parent_of` ends are now embedded (below):
  - a `parent_of` end the call creates whose origin-framed context is over
    16 KiB, the rule the call's own concepts already had;
  - a call whose only new concepts are `parent_of` ends, when the session's
    embedding contract does not match the live embedder (such a call used to
    apply keyword-only);
  - a call that would embed more than 256 items (`MAX_HYBRID_EMBEDS`): new
    concepts plus the `parent_of` ends it creates. On the asynchronous MCP path
    this is refused at the call, not on the receipt. Split the call. A store
    without vector search embeds nothing and is not limited.

- `LamboError` gains a variant, `ImageIdTaken(String)` (#22 PR 4): an
  image derive refused because a text concept already holds the image's
  caption and id. It replaces the `Embed` refusal PR 3 used for that case, so
  library code that matches `LamboError` exhaustively needs the new arm. Its
  only field is the caller's image id, and every surface shows it with the
  fix (choose another image id); the ledger's `error_kind` for it is
  `"image id taken"`.
- `Config` and `EmbedderConfig` each gain a public field,
  `accept_client_vectors: bool` (#22 PR 4). Code that builds either with a
  struct literal must add it (or use `..Default::default()`); both default to
  `false`, and `lambo.toml` files are unaffected.
- `EmbedderKind` gains a variant, `EmbeddingGemma2` (#22 PR 5), so library
  code that matches it exhaustively needs the new arm. `EmbedderConfig`
  gains a public field, `images: Option<bool>`; code that builds one with a
  struct literal must add `images: None` (or use `..Default::default()`).
  `lambo.toml` files are unaffected.

- `lambo serve-web` checks `Host` while no credential is configured (#4
  PR 2, a DNS-rebinding fix, see Fixed): no `LAMBO_AUTH_TOKEN` or
  `--auth-token`, no `[[web.credential]]`, nothing inherited (PR 3). An
  unauthenticated window reached under any name other than `localhost`,
  `127.0.0.1` or `[::1]` now answers `403`: a reverse proxy that forwards
  its public name as `Host` (Caddy's default) must pass that name with
  `--allowed-host` (or `[web] allowed_hosts`). A window with any credential
  accepts any `Host`, as before.
  `scripts/aws-infra/launch_exhibit_ec2.py` now does: with `--hostname` the
  service passes `--allowed-host <hostname>`, and with `--self-signed` Caddy
  sends the upstream address as `Host`. An exhibit launched before this
  change keeps its old user data, so before it runs a build with the check,
  add `--allowed-host <hostname>` to the `exec` line of
  `/usr/local/bin/lambo-serve-web` (self-signed: add
  `header_up Host {upstream_hostport}` under `reverse_proxy` in
  `/etc/caddy/Caddyfile`), or relaunch it.
- `lambo::cli::serve_web::Args` gains `sessions: Vec<String>` and
  `allowed_hosts: Vec<String>` (#4 PR 2). Code that builds it with a struct
  literal must add both (`Vec::new()` keeps one session and no extra host).
  `lambo serve-web --session` is now repeatable and no longer required when
  `[web] sessions` names one; with neither it still exits `2`, after reading
  `lambo.toml`.
- `lambo::cli::serve_web::Args` gains `credentials: Vec<WebCredential>`, and
  `lambo::config::WebConfig` gains `list_sessions`,
  `inherit_serve_credentials` and `credentials` (#4 PR 3). Code that builds
  either with a struct literal must add them (`Vec::new()` and `false` keep
  today's behaviour; `WebConfig` also takes `..Default::default()`). Code
  that parses `lambo.toml` is unaffected.
- A `lambo.toml` with `[[web.credential]]`, or with `[web]
  inherit_serve_credentials = true` beside `[[serve.credential]]`, makes
  `lambo serve-web` require a bearer token on every request, loopback
  included, and turns its `Host` check off (#4 PR 3), as one token does
  today. Without either key nothing changes.
- A `LAMBO_AUTH_TOKEN` or `--auth-token` longer than 4 KiB refuses the start
  of `lambo serve-web` (exit 2, naming the bound, never the value) (#4 PR 2).
  The window now authenticates through the shared credential set, which
  refuses a longer presented token before comparing, so such a token could
  never have been accepted. So does one with leading or trailing whitespace
  or a byte outside printable ASCII, which no request can present (the
  presented credential is trimmed, and such a header value is unreadable),
  so the window used to answer every request `401` with no hint why. The
  rule and its messages are now one validator shared with `lambo serve`.
  A request carrying two `Authorization` headers now gets the `401`, as
  `lambo serve` answers it; before, the first header decided.

### Changed

- `lambo serve-web` sends two security headers (#92). Every response the
  router answers, including a Host `403`, a bearer `401`, the uniform
  `404`, a `405`, a `308`, `/healthz`, JSON and the assets, carries
  `X-Content-Type-Options: nosniff`. Hyper's own replies to a request it
  cannot parse (a malformed request line, a `431` for oversized headers)
  never reach the router and carry neither header. The HTML page (`GET`
  and `HEAD` of `/` and `/s/<session>/`) also carries
  `Content-Security-Policy: default-src 'none'; script-src
  'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src
  'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none';
  object-src 'none'`. `img-src` is `'self'` only: the page has no image,
  no CSS `url(` and no `data:` URL, so the draft's `data:` is not in the
  policy. There is no `'unsafe-inline'` and no `'unsafe-eval'`. JSON, the
  assets, `/healthz`, redirects and errors do not carry the policy.
  Relative to the commit this change is based on, the responses compared
  in `dev-diary/notes/feature-92-portal-csp.md` match apart from these two
  headers, and the layer does not rewrite bodies.
- Daemon recency in a session younger than 10 minutes no longer treats that
  short span as the whole `[0, 1]` range (#95). The denominator is
  `max(span, 10 minutes)` in integer milliseconds, clamped to `[0, 1]`, so a
  stall of tens of milliseconds cannot flip a real query gap. A session at
  least 10 minutes long scores bit-for-bit as before, including one whose
  span is exactly 10 minutes. Garbage-collection eviction recency is
  unchanged. A session with no interactions scores recency `1.0`, as
  before. `lambo demo` keeps a 10ms wall pause and widens its script clock
  to 60 seconds a step. Each close also reads that clock, and two closes
  come before act III, so the twelve interactions are calls 0–8, 10, 11
  and 13: a 13-minute span, above the floor, with the positions the old
  10ms step gave them. The demo's recency values, P90 candidate set,
  printed GC headroom and recall output are what they were. Because the
  script clock now spans minutes, the demo's `conflict_recency_window` is
  30 seconds plus that span (810 seconds), so agent B's recall still
  carries all eight warnings, including its own conflict lines on `redis
  backend` and `middleware/session.rs`.

- A tool call that fails because a vector read refused its probe (the
  session's embedding contract changed mid-query, or the vector width
  differs) now tells the caller to re-check the session's embedding
  contract (`lambo_stats`) and retry, instead of a bare `store error (the
  detail was logged server-side)` (#22 PR 6 review). The text is fixed and
  echoes nothing from the refusal; the class, and the ledger's
  `error_kind`, stay `store error`. A recall by image or query vector is
  where this is usually seen, since it fails rather than degrade.

- `lambo_recall`'s published schema changes additively (#22 PR 6): two
  optional properties, `image` and `query_vector`, and `query` is no longer
  in `required` (it gains `"default": ""` and a description saying it is
  required unless an image or a query vector is sent; the server still
  refuses a call with none of the three). The tool-list golden is updated
  for `lambo_recall` only; the other seven schemas are byte-identical.
  `lambo recall --query` is likewise optional beside `--image` or
  `--query-vector-json`. A `lambo_recall` call with no `query` (and neither
  of the two) now gets a tool error (`isError`), `query must be a
  non-empty string (or send image or query_vector)`, where before serde
  refused it as a JSON-RPC invalid-params error (`missing field query`).
- `lambo serve-web` serves an allowlist of sessions (#4 PR 2): the ordered
  union of the repeatable `--session` and `[web] sessions`, each at
  `/s/<session>/` (data routes under `/s/<session>/api/...`), the first also
  at the unscoped `/` and `/api/...`, which answer exactly as before. There
  is no store discovery. With more than one session every name must be 1 to
  128 bytes of `[A-Za-z0-9._:-]` (exit 2 naming the one that is not); one
  session keeps `--session`'s looser rule. A session that is not served, a
  malformed, percent-encoded or oversized id and an unrouted path answer the
  same empty `404` on every method, before any store read; with any
  credential configured the `401` comes first and is the same for every
  id. A request other than `GET` or `HEAD` on a served
  session's route is `405` (`HEAD` is answered like `GET`). A served session never written, or erased, is an
  empty page (`200`). The scoped page carries `no-store` and
  `Referrer-Policy: same-origin`. Startup prints one more line, naming the
  credential and how many sessions it reads. The page's script fetches
  relative `api/...` URLs, so the page at `/s/<session>/` reads that
  session and the page at `/` the default; `GET` or `HEAD` of
  `/s/<session>` without the slash is a `308` to `/s/<session>/` (the query
  kept), so the relative URLs resolve under it. (#4 PR 4 added the page's
  session picker.) The page sends no `Authorization` header, so with any
  credential configured a plain browser cannot open it at all: it needs a
  proxy that adds the header for each user (design 4.4; a login flow is
  out of scope).
- `lambo.toml` `[serve]` `attach_concurrency`, `idle_detach_secs` and
  `per_session_rps` are enforced by an HTTP `lambo serve` (#32, sixth
  part), so the startup notice no longer names them; none applies to a
  stdio serve, which does not name them either. The notice is now logged
  only by an HTTP serve whose table sets `[[serve.projects]]`.
- The startup warning about credentials (#32, fifth part) now names a
  credential without `create` that reaches sessions the serve does not pin
  (#32, sixth part): it attaches one only once the session exists. The
  warnings about unpinned names being unreachable are gone, since they are
  reachable on demand.
- Every session of an HTTP serve with several sessions, or with sessions
  attached on demand, draws its own request bucket at `[serve]
  per_session_rps` (default `--rate-limit-rps`, burst twice that) after the
  credential's (#32, sixth part); a request over it gets `429` with
  `Retry-After: 1`, as the credential's limit answers. A serve of one
  session that attaches nothing on demand draws one only when
  `per_session_rps` is set, so it answers as before, each credential at its
  full `--rate-limit-rps`.
- A multi-session `lambo serve` whose pinned session was erased now starts
  and serves the other sessions, answering the erased one with the erased
  error (MCP error `-32003` to a `POST` of one JSON-RPC request with an
  `id`, `initialize` included; `410` to anything else); it used
  to refuse to start. A pinned session found erased on a background retry
  answers the same, instead of the `503` for a session that needs an
  operator (#32, seventh part). An on-demand session found erased (#32,
  sixth part) gets the same answer, where it got `410` alone.
- `lambo serve-web` reads each session it serves through a shared
  per-session view (#4 PR 1). Every request and every open tab reads one
  load of the session until it is older than `[web] view_ttl_ms` (1.5 s by
  default, the page's poll interval), so the store sees about one load per
  session per 1.5 s however many tabs are open, and concurrent requests for
  a stale view share one load. So the page is staler than before: it can
  show durable state up to `view_ttl_ms` older than a fresh store read
  would (`0` reloads on every request); the writer-published flush lag is
  still read on every poll. An erased session's content stays at most one
  `view_ttl_ms`.
  Startup checks the store schema and no longer loads the session: a store
  error other than an unprovisioned schema now shows on the first request
  rather than at startup, and the embedding-mismatch warning is printed at
  the session's first load, naming the session. Routes, payloads, status
  codes and `no-store` are otherwise unchanged; `/api/recall` validates its
  arguments before touching the store, as before.
- A hybrid derive whose embedding text would exceed 16 KiB is refused when
  it is called, not on its receipt (#74): each new concept and `parent_of`
  end is embedded framed with the whole call's text, so on a store with
  vector search a concept sent alone may be at most 8,189 bytes.
  `lambo_derive` and `lambo_derive_image` refuse it as a bad parameter,
  `lambo derive` and `derive-image` as a usage error, and `Memory`'s derive
  calls with `LamboError::Config`, each naming the part that is too long.
  The check runs before matching is known, so an over-long concept that
  would have matched is refused too. The `lambo_derive` tool description
  states the limit.
- `lambo_derive`, `lambo_record_action`'s `action`, `lambo derive`
  (`--content`, `--concept`) and `lambo record-action --action` refuse text
  holding a token that reads exactly `[image:<id>]` (a valid image id) once
  canonicalized, the way the concept key reads it (#22 PR 4). Prose that only
  mentions `[image:` is accepted. Only `lambo_derive_image` builds that
  suffix; a text concept holding it could take an image's identity before the
  image is derived.
  References (`parent_of` ends; `produces`, `modifies`, `depends_on`) still
  accept it, since naming an image concept's content is how a text write
  links to it.
- A write that fails because its image id is taken now says so, with the
  fix and the caller's image id, on the tool error, on the receipt and after
  a replay, instead of "embedding error (the detail was logged
  server-side)" (#22 PR 4). Every other failure reads as before.
- `lambo_stats` (and the I2 heartbeat payload) gains `embedding_contract`
  and `embedding_modalities`, and its text summary an `embedding:` line
  (#22 PR 4). Additive.
- A `[[serve.projects]]` `path` must now be absolute or start with `~/` (or
  be `~`); a relative path or `~user` is refused when `lambo.toml` is read,
  naming the entry (#32).
- The write queue's probe log lines (`write queue: bounds are static ...`
  and `the embedder could not be probed`) gain a `scope` field, `session`
  or `process`, beside the `session=<id>` they already carried (#32).
  `lambo serve`'s probe is process-wide now: with `scope=process` the
  figures are the embedder's for every session in the process, and
  `session` names the session whose attach started the probe (in a
  one-session serve, its session, as before). The failure warning says
  who goes without probe telemetry. Filters on `session=` keep matching.
- `lambo.toml` parse errors no longer quote the offending line. They give
  the parser's message and a line and column instead, so a misspelled key
  next to a secret (a DSN with a password, say) no longer prints the secret
  into a startup log. A script that matched the old `TOML parse error at
  line N` text must match `(line N, column M)` instead.
- An unknown `[store] kind` or `[embedder] kind` (in `lambo.toml` or
  `LAMBO_STORE` / `LAMBO_EMBEDDER`) no longer quotes the value: the error
  lists the accepted kinds with `(value not shown)`. A wrong-typed or unknown
  enum value in `lambo.toml` reads `string (value not shown)` or
  `unknown variant (value not shown)`, and an unknown `promotion_policy` is
  quoted only when it is a short word. A DSN or token pasted under the wrong
  key no longer reaches a startup log.
- `lambo.toml` `[serve]` `sessions`, `default_session` and `max_attached`
  are enforced by an HTTP `lambo serve` (#32, fourth part). The startup
  notice reads `[serve] sets keys this serve ignores` and
  names only keys the serve does not act on; after the sixth part that is
  `[[serve.projects]]` on an HTTP serve (only a stdio serve reads it, #32
  eighth part), and it is not logged for a table that does not set it.
  With several sessions, `--ledger-heartbeat` writes one `stats` line per
  session per interval.
- The `[serve]` startup notice no longer lists `[[serve.credential]]`, which
  an HTTP serve enforces (#32, fifth part); a stdio serve authenticates
  nobody, ignores the credentials and does not read their variables. The
  notice drops its "this serve still authenticates only with --auth-token"
  clause.
- A request whose credential does not reach a session gets the same empty
  404 as an unhosted session or an unrouted path, whatever state the session
  is in (#32, fifth part). Before, anyone holding the one token could tell a
  hosted session that was held elsewhere, detaching or failed (503) from a
  name the serve did not host (404). The credential is checked in memory
  before the session is looked up, and no store call is made for a refused
  request.
- `LAMBO_AUTH_TOKEN` is now read, and an empty one refused (exit 2), before
  `lambo serve` builds its backends, so the refusal no longer waits for a
  model to load.
- `lambo serve` warns at startup when `LAMBO_AUTH_TOKEN` (or
  `--auth-token`) is set beside `[[serve.credential]]` (#32, fifth part);
  the warnings about credentials that reach unpinned sessions are the sixth
  part's, above.
  A refused bearer token is logged at WARN once per 10 seconds, with a
  count of those held back, and at DEBUG otherwise.
- With `--ledger`, a `call` line made through a configured credential
  carries `credential`, its name (#32, fifth part). Additive; lines for the
  legacy `default`, the implicit `local` and stdio are unchanged.
- Every `serve --ledger` line now carries `session` (#32). `startup` and
  `lease` lines always did; `call`, `completion` and `stats` lines gain it so
  one ledger file can hold every session of a multi-session serve.
  Additive: `v` stays `1` and no existing key changes.
- The write queue's startup probe now times a representative write instead
  of one embed (#11), so `write_queue_probe_serial_items_per_sec` and
  `write_queue_items_per_sec` read in writes per second, the unit
  `write_queue_serial_items_per_sec` has always used once your own writes
  are observed. A `lambo_derive` embeds each concept together with the text
  of every concept in the call, so a write costs several embeds and its cost
  grows faster than its concept count. The probe now embeds exactly what a
  two-concept derive of about 340-byte concepts embeds. Expect the probe
  figures to read about half what they did. Measured on an M3 Pro with the
  candle Metal BGE-M3 and a scratch SQLite store: probe 11.4 to 11.6 against
  an observed 2.3 to 2.8 writes/s before (4.1x to 5.0x apart), 5.7 against
  the same observed rate after (2.0x to 2.5x; what is left is derives larger
  than the representative one). The drain rate itself is unchanged: after
  #8 a write's time is its embeds. An embedder that answers one request at
  a time may not finish the probe's four-wide leg in its 5 s budget; the
  serial figures are then still published, and `write_queue_items_per_sec`
  is `null`, where the whole probe used to read `unmeasured`. A probe that
  measures nothing logs which leg failed and which budget ran out.
- The longest `lambo_stats` `wait_ms` is now 34000 (was 4000), and the
  published schema says so (#11). It covers the longest one write can take
  to apply (the 30 s hybrid I/O deadline plus two 2 s drain budgets), so a
  wait that ends `pending` now means other writes were queued ahead of it.
  On the Metal rig a three- or four-concept derive took up to 4.6 s to
  apply, so a read-your-writes wait could answer `pending` about a healthy
  write. A wait still returns the moment the write settles, and at most 16
  waits run at once, of which one `agent_id` holds at most 8: a wait over
  either cap answers at once with the receipt's current state. A wait also
  answers once the session closes, so a wait on a `pending_replay` receipt
  no longer outlives the server's shutdown; a wait on a write the close
  defers answers `intent_durable`, not `pending`.
- `flush_lag_ms` (in `lambo_stats`, the heartbeat and the stats a reader
  process reads from the store) is now the time since the store last held
  every write, not the time since the last successful flush (#16 §3). An
  idle writer reads under 100 ms instead of its idle time: the dogfood rigs
  read 13.7 minutes, 12.1 hours and 48 hours while `log_depth` was 0 in
  every snapshot. With writes waiting on a store that is not taking them it
  grows as before, counted from the moment the last successful flush
  drained its batch, so it covers writes that landed while that flush was
  still running. The key, type and unit are unchanged. The stats row a
  reader process reads is now republished between flushes when it has
  drifted (at most every 5 s); it used to change only when a flush was
  attempted, so it never showed an idle writer's lag and froze through the
  10 s pause after a failed flush. A degraded session stops republishing
  it. A batch the store rejects outright resets the lag although it never
  landed; `dead_lettered` is what counts it.
- On SQLite, the process that holds a session (`lambo serve`, or an embedded
  `Memory`) now ranks recall's vector leg and hybrid `derive`'s semantic match
  against the vectors its in-memory graph already holds, instead of reading,
  decoding and re-parsing every stored vector from the database on each recall
  and once per unmatched concept on each derive (#8). Rankings over flushed
  data are unchanged, bit for bit: same candidates, order, scores, ties and
  refusals. Measured on a 3,600-concept session (fixture embedder, release,
  macOS): warm recall p50 219 ms to 4.3 ms, a one-concept derive acknowledged
  to applied 246 ms to 14 ms, a three-concept derive 670 ms to 20 ms. No
  memory is added: the scan borrows the vectors the graph holds.
  - A concept is a vector candidate as soon as it is written, before the
    write-behind flush (which can lag by minutes), so recall finds it and a
    later `derive` can merge a near paraphrase into it. A removed concept stops
    being a candidate at once.
  - Readers without a live session (`lambo recall`, `lambo serve-web`) still
    scan the database. Postgres and CockroachDB keep their database-side
    search: they score by database distance, so their ranking would change.
  - A derive no longer competes with the flush for SQLite's single connection
    while it matches, and the matching scan no longer spends the derive's 30 s
    store-I/O deadline: it runs in memory, about 0.8 µs per stored vector at
    1,024 dimensions.
- A session holder (`lambo serve`, or an embedded `Memory`) now keeps the
  query vectors of its recent recalls, so a repeated recall of the same text
  no longer calls the embedder (#14). The vector depends only on the text and
  the embedder, not on the graph, so a write between two identical recalls
  still reuses it; the recall itself runs in full every time, so results are
  unchanged. Measured with candle BGE-M3 on Metal over a 3,600-concept SQLite
  session (release, macOS): a repeated warm recall p50 22.3 ms to 4.1 ms, and
  the same recall with a derive applied between each pair 21.5 ms to 3.6 ms.
  A novel query still pays its embed (21.9 ms to 21.8 ms).
  - The cache belongs to one session and is never shared across sessions in a
    process, so reply timing cannot reveal another session's queries.
  - Bounded at 128 entries and 1 MiB per session (about 560 KiB for short
    queries at 1,024 dimensions), least recently used first out. A failed
    embed is not cached. A closed or erased session refuses the recall before
    the cache is consulted.
  - A vector already cached is reused while the embedder is unavailable, so a
    repeated query keeps its vector leg through an embedder outage instead of
    degrading to keyword-only with the `vector_degraded` warning.
  - `lambo recall` (one recall per process) is unchanged.

### Added

- Documentation for multi-session serving (#32, ninth part). The command
  line reference's "Several sessions in one serve" is now one overview:
  pinned vs on-demand sessions, who reaches which session, one table of
  every bound with its default and its answer, the admin routes and
  shutdown. The configuration reference gains a `[[serve.credential]]` key
  table and corrects the HTTP transport's limits (the rate limit is per
  credential; the MCP-session cap spans every session, has a
  per-credential share and answers `Retry-After: 5`).
  `dev-diary/notes/feature-32-multi-session.md` records the design and
  every decision and deviation across the parts. The spec's single-writer
  rule now reads "one writer per session; a process may hold many"
  (`dev-diary/notes/spec-deviation-2-2-many-leases.md`; the spec itself is
  frozen). The dogfood runbook describes the move to several sessions,
  which the rig has not made. `scripts/docs/check-mirror-drift.sh` now also
  gates `config.mdx`, and reports every drifted page rather than stopping
  at the first.

- EmbeddingGemma 2 embedder over llama.cpp (#22 PR 5): `[embedder] kind =
  "embeddinggemma2"` (aliases `embeddinggemma-2`, `eg2`), feature
  `embed-eg2`, which released binaries (`ship`) now carry. One
  `llama-server` (b11452 or later) embeds text and, when started with
  `--mmproj`, images into one space, so `lambo_derive_image` can send the
  image itself. Lambo adds the model card's task prefixes (documents
  `title: none | text: `, recall queries `task: search result | query: `,
  images none), truncates to `dim` (768, 512, 256 or 128) and
  re-normalizes. The contract `model` is the weights artifact plus the
  prompt profile, by default
  `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2`, since
  `llama-server` ignores the request's model name. The profile sends every
  image in a canonical form: Lambo decodes each PNG, JPEG or WebP and sends
  a lossless PNG whose longer side is exactly 768 px (Lanczos3 down,
  Catmull-Rom up, aspect ratio kept; the exact rule is in the
  configuration reference). A flat image then embeds bit-identically at
  any submitted size from 128 to 3000 px; a patterned one stays close but
  not identical (measured on b11517: at least 0.9991 cosine between renders
  at 768 px and above, 0.982 for a checkerboard submitted at 128 px), since
  resampling changes its pixels. **Size invariance has a limit:** small
  images with sharp detail can drift by up to about 2%, so if bit-identical
  vectors matter (comparing vectors directly, deduplicating), always send
  the same rendition of an image, such as the original file. A WebP is
  never sent as WebP (`llama-server` decodes it only through an external
  `ffmpeg`). Decoding is
  bounded (4096 px a side, two at a time), and an image Lambo cannot decode
  is refused as unreadable before the server sees it. The canonical pixels
  are pinned by golden tests: if a dependency update moves them, that ships
  as a new profile name. Downscaled and JPEG cases are pinned to within one
  step per sample, since they are not bit-exact across platforms; that
  wobble is not a new profile. The `embed-eg2` feature now pulls in the `image`
  crate (PNG, JPEG and WebP only) for this. The profile was `lambo-eg2-v1` during
  development, without the canonical form; nothing was released with it, so
  there is no migration. The profile fixes the
  image budget at 280 tokens: start the server with `--image-min-tokens 280
  --image-max-tokens 280 --batch-size 8192 --ubatch-size 8192` (at the
  default ubatch of 512 the server silently caps the budget to 256). Lambo
  checks the budget by embedding a reference image of its own and refuses
  images when the server reports another token count for it. Before its
  first embed, and again every 60 seconds and after a failed embed request,
  the adapter asks `/props` which file the server loaded and whether it has
  vision, and refuses a file that is not EmbeddingGemma 2, another
  quantization than the artifact names (llama.cpp's `Q4_K - Medium` matches
  `Q4_K_M`), or images on a server without a vision projector. An image
  sent to such a server anyway is a permanent configuration error naming
  `--mmproj`, and an image the server cannot decode a content refusal, not
  the transient error their HTTP 500 used to mean. `[embedder] images =
  false` (default on) makes it text-only. `api_key_env` works with this
  kind as with `bge_m3`. See `lambo.example.toml` for the full server
  command line.
- Recall by image or by a client query vector (#22 PR 6, the Dresscode
  "close to the one you dismissed" path). `lambo_recall` takes an optional
  `image` (mime and base64, embedded by the server with
  `Embedder::embed_image`, no prompt) or `query_vector` (values and the
  contract they are in), at most one; with either, `query` is optional and
  still feeds the keyword leg, and the vector leg searches by the image or
  vector. With no text the recent-interactions leg is skipped: its flat
  0.35 was calibrated on text and would rank whatever was derived last
  above a true image match scoring lower; with text it runs as for any
  recall. The same checks as `lambo_derive_image`: base64 capped before it
  is decoded, MIME matched to the magic bytes, a client vector accepted
  only with `[embedder] accept_client_vectors = true` and only in exactly
  the session's contract, refused with no echo. Not cached (the #14 query
  cache holds text queries only), never logged; the ledger line gains only
  `by: "image" | "vector"`. A store without vector search, an image the
  embedder cannot embed, or a failed vector read is an error rather than an
  answer without the vector leg, and
  a structural phrasing beside an image is not dispatched to traversal.
  Library: `Memory::recall_by` with `recall::query_vector::QueryBy`;
  `surface::image::check_submitted_vector_as`; CLI `lambo recall --image
  PATH [--mime M] | --query-vector-json PATH`, under derive-image's file
  caps. Works over every vector source (#8's holder graph, the store's
  checked read on SQLite, Postgres and Cockroach, and #18's tier); no store
  contract changed.
- Documentation for the multi-session read-only window (#4 PR 5). The
  command line reference's "Watch a session in a browser" opens with an
  overview of several sessions in one window, set against "Several sessions
  in one serve" (#32): the window is a lease-free reader of exactly the
  served sessions, attaches nothing on demand, and is up to `view_ttl_ms`
  behind the store. `lambo.example.toml` explains every `[web]` key with its
  default, and `lambo serve-web --help` says "credential" where it said
  "token". `dev-diary/notes/feature-4-multi-session-portal.md` records the
  design and every decision and deviation across the five PRs, and closes
  hardening task H7.
- Per-credential read scope for `lambo serve-web` (#4 PR 3):
  `[[web.credential]]` entries (`name`, `token_env`, `sessions` with `"*"`
  for every served session, `session_prefix`), each a bearer token that
  reads only the served sessions in its scope, beside the `default`
  credential from `LAMBO_AUTH_TOKEN` / `--auth-token`, which reads them all.
  The grammar is `[[serve.credential]]`'s, shared in one module, minus the
  write capabilities: `create`, `erase` and `admin` are refused by name,
  whatever their value. `[web] inherit_serve_credentials = true` also
  accepts every `[[serve.credential]]` token as a read credential with its
  scope, its capabilities dropped; an inherited `"*"` is what `"*"` reaches
  on `lambo serve` (the `[serve] sessions` plus every session under a
  `[[serve.credential]]` prefix), never every served session. A session outside the caller's scope is
  the same empty `404` as an unserved one, before any store read; the
  unscoped `/` and `/api/...` serve the default session only to a credential
  that may read it. Tokens are read at startup, before any backend, and a
  missing variable, an unpresentable token, or a token shared by two
  credentials or with `LAMBO_AUTH_TOKEN` exits `2` naming the credential,
  never a value. Startup prints one count line per credential and warns
  about a legacy token left beside configured credentials, a credential
  naming sessions that are not served, and a credential that reads no
  served session; it also notes an `inherit_serve_credentials` that imports
  nothing (saying so plainly when that leaves the window unauthenticated
  on loopback). `[embedder] api_key_env` may not name a
  `[[web.credential]]` variable. The page still sends no `Authorization`
  header, so per-credential scope serves a proxy that adds it, and scripts.
- `[web] list_sessions = true` (#4 PR 3): `GET /api/sessions` answers
  `{"sessions": [...]}` (`no-store`, no store read) with the served sessions
  the presenting credential names exactly, in served order, or every served
  session for `default`, `"*"` and the unauthenticated window (startup
  warns about the last). It never expands a prefix or shows another
  credential's names. Off (the default), the route does not exist and is
  the same empty `404` as any unknown path; under `/s/<session>/` it is
  always that `404`.
- A session picker on the `lambo serve-web` page (#4 PR 4). It appears only
  when the caller reads more than one served session: `/api/session` then
  carries `"switchable": true`. The field is absent otherwise, so for a
  single-session window the `/api/session` JSON and the response headers
  are unchanged and the page looks the same; the page's HTML gained the
  picker's markup, hidden. With `list_sessions` on it offers the listed
  names plus "Other session…", a text field for a session reached through
  a `session_prefix` (the listing never expands one); otherwise it is a
  text field plus the names this browser profile has opened
  (`localStorage` `lambo-sessions`, names only, shared by whoever uses the
  profile). Switching asks for the target page with `HEAD`, then navigates to
  `/s/<session>/`; a name the caller
  cannot read (the portal's `404`) is reported in place and dropped from
  the history, while a `401`, a `5xx` or no answer keeps it and says to
  try again. A long name is cut with an ellipsis on a narrow screen. A
  session with nothing in it says "No memory in this session yet" instead
  of implying concepts are being recorded, judged by the poll's counts so
  it follows a first write or an erase within one poll.
- `[web] sessions` and `[web] allowed_hosts`, and the repeatable
  `lambo serve-web --allowed-host` flag (#4 PR 2). A refused session name or
  host is quoted (neither is a secret); an empty, repeated or
  control-character session name and a host that is not an HTTP authority
  are refused when the file is read.
- On-demand sessions for `lambo serve --transport http` (#32, sixth part). A
  request for an unattached session inside a credential's scope attaches it:
  always for a credential with `create = true`, and for one without only
  once the session exists (it has a lease row); otherwise the `404`.
  Concurrent first requests share one attach. An erased session answers a
  `POST` of one JSON-RPC request with an `id` (`initialize` included) with
  the erased error (MCP error `-32003`) and anything else with `410`
  (seventh part), and is never recreated. At most `[serve]
  max_attached` sessions are attached: at the cap the least recently used
  on-demand session with no tool call running is detached to make room, else
  `503` with `Retry-After: 5`. A session idle (no tool call) for
  `idle_detach_secs` is detached. A detach flushes and releases the lease,
  keeping the fencing token, so a reattach takes the next one. A session
  another writer holds answers `503` with a `Retry-After` of when its lease
  could lapse. At most `attach_concurrency` attaches run at once (one on
  SQLite). Pinned sessions are never evicted or idle-detached, and the
  shutdown closes and releases on-demand sessions with the pinned ones. An
  attach checks that the session exists, is not erased and is not held by
  another writer before it detaches anything to make room; a session with a
  request or call running, or used in the last 2 seconds, is never the one
  detached. The on-demand places are shared among the credentials that reach
  them (the places divided by their number, rounded down, at least 1): at
  the cap a credential at its share detaches only its own sessions. A `404`,
  `410` or failed attach is remembered for 30 seconds (at most 1,024
  sessions), never stopping a credential with `create`. A request waits at
  most 15 seconds for an attach, and an attach is abandoned (its lease
  released) after 60 seconds, as is a pinned session's background retry;
  both answer `503` with `Retry-After: 5`. At most twice `max_attached`
  attaches wait to start; past that a request for another unattached session
  gets `503` with `Retry-After: 5`. A startup warning names a `max_attached`
  that leaves no on-demand place, and a library `idle_detach` under a second
  is refused.
- `[web]` in `lambo.toml` (#4 PR 1): `view_ttl_ms` (1500, 0 to 60000),
  `max_loaded_sessions` (4), `load_concurrency` (2, 1 to 1024, always 1 on
  SQLite) and `recall_concurrency` (4, 1 to 1024) bound the read-only
  window's views. Unknown keys and out-of-range values are refused when the
  file is read, naming the key and never the value. An older binary refuses
  a file with `[web]`.
- `lambo serve-web` answers a recall that waits 2 s for one of the
  `recall_concurrency` slots with `503`, `Retry-After: 1` and `no-store`
  (#4 PR 1), and keeps a per-session query-embedding cache (#14's, 128
  entries or 1 MiB), so a repeated recall query skips the embed.
- MCP tool `lambo_derive_image` (#22 PR 4): one image concept per call, a
  caption plus either the image (`image`: mime and base64, embedded by the
  server on the call path) or a client-computed vector (`vector`: values and
  the embedding contract they are in). Acked with a receipt like
  `lambo_derive`; only the vector is queued, made durable or replayed. It is
  listed only when the server's embedder embeds images or client vectors are
  enabled, so a text-only deployment still lists exactly the seven tools,
  with byte-identical schemas (pinned by a golden). Base64 is capped at
  2,796,204 characters before it is decoded, also on stdio. A client vector
  must declare exactly the session's contract (AC4); a refusal names the
  differing fields and never echoes the vector, the declared strings or the
  image.
- `lambo derive-image` (#22 PR 4): the CLI twin, from `--image PATH`
  (`--mime` defaults to the file's type) or `--vector-json PATH`.
- `[embedder] accept_client_vectors` (default `false`) and its overlay
  `LAMBO_ACCEPT_CLIENT_VECTORS` (#22 PR 4): whether this process accepts
  client-computed image vectors. One setting for every session a server
  holds; a call with a vector while it is off is refused naming the key.

- `Embedder` gains three methods with defaults, so every existing adapter
  compiles and behaves unchanged (#22, first part):
  - `embed_query`: the recall query role. It defaults to `embed`, and recall
    now embeds its query through it. Every shipped adapter (BGE-M3, candle,
    Gemini, the fixture) keeps the default, so recall answers exactly as
    before; an asymmetric model can now give queries their own prompt.
  - `modalities`: what the adapter embeds, `Modalities::TEXT` by default.
  - `embed_image`: embeds an `ImageInput` into the same space; by default it
    refuses with `EmbedError::Unsupported`.
  A wrapper that delegates to another embedder should forward all three, and
  `as_any`; one that implements only `embed` inherits the defaults.
- `EmbeddingSource` (#22): where a concept's vector came from when it is not
  an embedding of its own content, persisted per concept in the new
  `concepts.embedding_source` column as compact JSON. It holds the modality
  (`image`), which side computed the vector (`server` or `client`), the MIME
  type when known and, for a server-embedded image, the hex sha256 of the
  bytes; never the bytes. Every store round-trips it, a read access leaves
  it alone, and erasing a session erases it with the concept row. When an
  embedding contract change quarantines a session's vectors, the sources
  are kept: the concept stays marked as image-sourced with no vector, so
  a later re-embed cannot give it a vector of its caption. A stored
  value this build cannot read fails the load rather than reading as
  unset. Image derives (below) write it.
- Image concepts in the library API (#22, third part):
  `Memory::derive_image_as` and `Memory::derive_image_async_as` take a
  `graph::image::ImageDerive` (caption, type, optional image id, the image
  bytes or a client-computed vector with its declared contract, `parent_of`,
  event time) and derive one concept whose vector is the image's, not the
  caption's. Its content is `"{caption} [image:{id}]"`, the id being 1 to 64
  characters of `[a-z0-9]` or, by default, 16 hex characters of the image's
  (or the vector's) sha256, so the same image derives onto one concept and
  two images with one caption stay two. Image bytes are embedded on the call
  path and never stored, queued or written to a durable intent: the queue and
  its replay carry the vector. A submitted vector must declare exactly the
  live embedding contract and have its width, finite values and a non-zero
  norm; the server renormalizes it. An image derive needs the `hybrid`
  strategy and a store with vector search, refuses an `observation` type,
  never semantic-merges, and no text concept ever merges into an image; an
  image derive whose caption and id a text concept already holds is refused
  rather than left without a vector. A text query reaches image concepts
  through the ordinary vector leg. A durable image intent replayed under a different live contract settles
  `failed`. Its receipt kind is `lambo_derive_image`; the MCP tool and CLI
  verb arrive in a later release.
- `lambo re-embed` handles image concepts (#22): a full migration refuses
  while an image concept still carries a vector (it cannot be recomputed
  from a caption), unless `--drop-image-vectors`, which nulls those vectors
  in the same transaction as the migration, keeps each image concept and its
  source, and reports the count; deriving the same image again restores the
  vector. `--missing-only` and the full migration never give an image concept
  a vector of its caption, and report the image concepts they skipped. This
  replaces the blanket refusal of any session holding an image source.
- `FixtureEmbedder` embeds images (#22): a PNG carrying a `lambo-label`
  text chunk embeds exactly as a text query for that label, and any other
  image as a vector seeded from its digest. `png_with_label` builds such a
  PNG in code for tests.
- `lambo::surface::image::validate` (#22): the image rule every surface will
  share, and the only constructor of `ImageInput`. The declared type must be
  exactly `image/png`, `image/jpeg` or `image/webp`; the bytes must be
  non-empty, at most 2 MiB (`MAX_IMAGE_BYTES`), and carry the magic bytes of
  the declared type (a mismatch is refused, never corrected); and the header
  must declare each side between 1 and 4096 px (`MAX_IMAGE_SIDE_PX`), read
  from the PNG, JPEG or WebP header without decoding pixels. Refusals never
  quote the bytes or the declared type. Nothing calls it yet: images reach
  no CLI verb or MCP tool in this release.
- An optional `[serve]` table in `lambo.toml` for multi-session serving
  (#32, first part): pinned `sessions`, `default_session`, `max_attached`,
  `attach_concurrency`, `idle_detach_secs`, `per_session_rps`, a
  `[[serve.projects]]` cwd map and `[[serve.credential]]` entries that name
  the environment variable holding their token. The first part parsed and
  validated it only; the later parts enforce every key, and the one
  startup warning left (`[serve] sets keys this serve ignores`) names `[[serve.projects]]` when an HTTP serve's table sets it,
  since only a stdio serve reads the map. `token_env` must be an upper-case
  variable name and must not look like a token; a value that fails is
  refused without being quoted. A
  malformed table, a session name that cannot be
  addressed by URL, an inline token, or more pinned sessions than
  `max_attached` stops every command. An older binary refuses a file that
  has `[serve]` (unknown key).
- A stdio `lambo serve` no longer needs `--session` when `lambo.toml`
  names one (#32, eighth part): it uses the `[[serve.projects]]` entry whose
  `path` contains its working directory (the longest such path wins), then
  `[serve] default_session`, and otherwise refuses with the same missing
  `--session` error and exit code 2 as before, now before any backend is
  built. The working directory and each entry are compared as real paths
  (`~` expanded, symlinks, `.` and `..` resolved, whole components only);
  an entry that does not exist never matches, a working directory that
  cannot be resolved falls back to `default_session`, and two entries for
  one directory naming different sessions are refused (exit code 1). With
  `HOME` unset or relative, `~` entries are skipped with one warning and the
  rest of the map applies. A global map only applies when the client passes
  `--config` or sets `LAMBO_CONFIG`; otherwise each project's own
  `./lambo.toml` is read.
  `--session` always wins, and a stdio serve still takes at most one;
  `--transport http` pins its sessions as before (`--session` and
  `[serve] sessions`) and never reads the map.
  The serve logs which entry it used by its configured path; nothing
  quotes the working directory. Library: `ServeConfig::select_stdio_session` (and `_with`, with
  the working directory and home injected), `SelectedSession`,
  `SessionSource`, `SessionSelectionError`, `MissingSession`,
  `SESSION_REQUIRED`. The startup `[serve]` notice no longer names
  `default_session` or `[[serve.projects]]` for a stdio serve; an HTTP
  serve still names `[[serve.projects]]`.
- Multi-session serving (#32, fourth part): one `lambo serve --transport
  http` holds several pinned sessions, named by a repeated `--session`
  and/or `[serve] sessions`. Each is served at `/mcp/s/{session}`, and `/mcp`
  serves the default (the first `--session`, else `[serve] default_session`,
  else the first pinned). Each session keeps its own lease, fencing token,
  graph, caches, write queue and local endpoint (so a stdio `serve
  --session <name>` proxies into it); the embedder, store and listener are
  shared, and `--max-sessions` counts MCP sessions across the process. An
  unhosted or malformed id gets the same empty 404 as an unrouted path. A
  session held by another writer at startup is answered with 503 and
  `Retry-After` and retried 5 to 6 s after each failed attempt; a session
  that loses its lease is detached and retried while the others keep serving.
  A one-session serve is unchanged, including exiting when it loses its
  lease. Shutdown closes every session concurrently inside the existing
  budget and releases every lease. Credentials and the operator surface are
  the fifth and seventh parts below; on-demand sessions are the sixth, above.
- Credentials for `lambo serve --transport http` (#32, fifth part).
  `[[serve.credential]]` entries are enforced: each names the variable
  holding its token (read at startup; an unset, empty or non-UTF-8 one
  refuses the start with exit 2 before any backend is built, naming the
  credential, never a value) and reaches the sessions in its `sessions`
  and/or `session_prefix`. `--auth-token` / `LAMBO_AUTH_TOKEN` becomes the
  credential `default`, reaching every pinned session; with no credential at
  all, a loopback serve keeps answering every request as the implicit
  `local` credential. A non-loopback bind is satisfied by any credential.
  The presented token is compared with every configured one in constant
  time, with no early exit. A configured token equal to the legacy one, or
  shared by two credentials, refuses the start without quoting either.
  `lambo::mcp::check_serve_credentials` runs those checks for a library
  caller. `create` lets a credential attach a session that does not exist
  yet (the on-demand sessions above, sixth part); `erase` and `admin` open
  the operator surface below.
- Erase a session from a running HTTP `lambo serve` (#32, seventh part):
  `POST /admin/s/<session>/erase` with `{"confirm": "<session>"}`, for a
  credential with `erase` that reaches the session. The answer is the same
  JSON report `lambo erase-session` prints (`200`; a repeat reports
  `already_absent`), `400` for a bad body or a confirm that does not repeat
  the id, `408` for a body that does not arrive in time, `409` when another
  process holds the session, `503` while it is being erased, attached or
  detached, when another session's attach does not finish within 10
  seconds (the erase waits no longer for it), or the serve is shutting
  down, and `500` when the store fails, saying whether the session is
  durably erased, untouched, or in an unknown state (a repeat is always
  safe). A credential without `erase`, or one
  that does not reach the session, gets the empty `404` of an unrouted path,
  and no store is consulted. A session the serve holds is fenced in the
  process, its MCP sessions ended and its handle closed (its tasks stopped
  and joined) without flushing or releasing the lease, then erased as the
  lease's holder, so no other writer can take it in between. While the
  erase runs its requests get `503` with `Retry-After: 1`; once erased, a
  `POST` of one JSON-RPC request with an `id` (`initialize` included) gets
  the erased error (MCP error `-32003`, as a proxy to an erased session
  answers), anything else (a `GET` or `DELETE`, a notification, a
  response, a batch, an unreadable body) gets `410`, and nothing recreates
  it.
  An erased on-demand session is remembered like the sixth part's other
  negative outcomes. A one-session serve answers the erase and then exits.
  The shutdown waits for an erase in flight within its close budget, and cuts
  one still running 8 seconds after it began closing the sessions short
  with an unknown outcome. Not an MCP tool: the tool list is unchanged. `GET
  /admin/sessions`, for a credential with `admin`, lists the sessions in
  its reach with their state and, for live ones, their size. Neither route
  counts against the MCP-session cap.
- `lambo::writeq::EmbedderCalibration` and `MemoryBuilder::calibration`
  (#32, third part): the write queue's startup calibration probe once per
  embedder for the whole process. Builders over one shared embedder that are
  given clones of one calibration probe it once; every later build fires no
  probe embed and reports the same probe figures in `lambo_stats`, while each
  session's observed rate stays its own. A probe that failed or was
  aborted is run again by the next build over that embedder, at most once
  per `writeq::PROBE_RETRY_BACKOFF` (60 s); a measured one never is. The
  calibration's owner aborts the probe (`EmbedderCalibration::abort`, or
  dropping the last clone). Without one, each build probes for itself as
  before. `lambo serve` now creates one per process and aborts its probe at
  shutdown stage 2, beside the keep-warm.
- `lambo::surface::session`: session-id validation for ids taken from a
  request (`parse_addressed`: `[A-Za-z0-9._:-]`, 1 to 128 bytes, no leading
  `.`, no percent-decoding), the in-memory authorization types the coming
  multi-session routes and the web portal share, and their one uniform 404.
  `--session` keeps its looser rule.
- `Ledger::for_session`, a handle onto the same ledger that stamps `session`
  on its lines.
- `lambo_stats` reports `write_queue_probe_optimism` (the startup probe's
  rate divided by the observed one, `null` until your writes have been
  observed) and `write_queue_apply_samples` with
  `write_queue_apply_ms_p50` / `_p90` / `_max`: acknowledgement-to-applied
  time over the last 256 applied writes, `null` before the first (#11).
  Together they show whether a deployment's derives fit the `wait_ms`
  maximum without joining the call ledger. Additive keys; nothing else in
  the payload changes.
- The `bge_m3` embedder can send a bearer token, so it reaches hosted
  OpenAI-compatible embeddings endpoints such as Cloudflare Workers AI (#21).
  - `[embedder] api_key_env` names the environment variable holding the token
    (overridden by `LAMBO_EMBED_API_KEY_ENV`, which also holds a name). The
    token never goes in `lambo.toml`: an inline `api_key` is refused, and so is
    an `api_key_env` that is not an upper-case variable name or looks like a
    token, without quoting the value.
  - `api_key_env` may not name `LAMBO_AUTH_TOKEN` or any
    `[[serve.credential]]` `token_env`, which hold `lambo serve`'s own
    credentials, so the embeddings endpoint is not sent serve's token. The
    `LAMBO_AUTH_TOKEN` rule is the one `token_env` already had, now shared in
    one place. Neither `api_key_env` nor `token_env` may name a variable
    lambo reads its own credentials from: `LAMBO_COCKROACH_DSN`,
    `LAMBO_POSTGRES_DSN`, `DATABASE_URL`, `GCP_LAMBO_CREDENTIALS`,
    `GOOGLE_APPLICATION_CREDENTIALS` or `LAMBO_GEMINI_CREDENTIALS`.
  - The variable is read at startup. Unset or empty is a hard error naming it,
    never a request without a key. With a token, every embed request carries
    `Authorization: Bearer <token>`; without `api_key_env` no `Authorization`
    header is sent, so local `llama-server` setups see no change. `Debug`
    output shows only that a token is set. When an error body is quoted into
    an error, every run of 8 or more consecutive characters of the token is
    replaced, in its raw, JSON-escaped and percent-encoded forms; a shorter,
    case-changed or otherwise re-encoded echo is not detected. A quoted body
    is cut at 8 KiB, and a `2xx` body that does not parse is not quoted at
    all. Logs and errors print the embedder URL as scheme, host, port and
    path, never userinfo or a query. A `401` or `403` stays a permanent
    configuration error.
  - Redirects are never followed: a `3xx` is a permanent configuration
    error naming the status, not the redirect target, so the token is never
    resent to another URL. Plain `http` to a loopback host ignores
    `HTTP_PROXY` and friends, so a loopback token never reaches a proxy;
    `https` still honours them (the proxy only tunnels TLS). A base URL with
    a query or fragment is refused, since the embeddings path is appended to
    it.
  - A token is sent only over `https`, or over plain `http` to a loopback
    host (`localhost`, `127.0.0.0/8`, `::1`). `api_key_env` with a plain
    `http` URL to any other host is refused at startup, naming the host,
    before the variable is read. Configs without `api_key_env` are
    unaffected.
  - `api_key_env` with any kind other than `bge_m3` is refused.
  - `kind = "openai"` is an alias of `bge_m3`. It still reports and stamps
    `bge_m3`, so no existing session's embedding contract changes. Set `model`
    to the hosted id (for example `@cf/baai/bge-m3`): a hosted model is a
    different embedding contract from a local GGUF of the same model.
  - `lambo.example.toml` and the configuration reference document the Workers
    AI setup. The adapter's `check_health` remains llama.cpp-only.
- An optional Elasticsearch recall tier (#18, feature `recall-elastic`): a
  top-level `[recall]` section in `lambo.toml` wraps the configured store in a
  `TieredStore`. The store stays the source of truth and keeps leases, fencing,
  canonization and graph queries; each committed flush is mirrored to a
  per-embedding-contract index at an external version built from the fencing
  token and a per-session flush counter, and the index serves the vector leg of
  recall. A mirror failure never fails a flush: the session is marked stale,
  vector recall falls back to the store's own read (or to the keyword and
  recent legs when the store has no vector search), and the lease holder
  repairs the index at its next load or flush. A per-session sync marker in the
  index makes a crash between commit and mirror visible at the next load.
  `erase-session` also removes the session from the index and does not report
  success until a refreshed count finds nothing of it. The API key is by
  reference only (`api_key = { env = "NAME" }`). A build without the feature
  refuses a `[recall]` section by name. See
  `dev-diary/notes/feature-18-elastic-tier.md`.
  Hardened in review: every delete-by-query refreshes first and retries
  version conflicts; hits are re-scored with exact cosine from their stored
  vectors (16 extra fetched) and indices pin float `hnsw`; repairs run in the
  background, one per session, with deadlines (mirror 15 s, repair 10 min,
  delete-by-query 300 s); reads skip the index for 30 s after 3 failed or slow
  reads; a marker ahead of a load is re-checked, never repaired from; the
  marker `_id` is the SHA-256 of the session id; unleased writes are not
  mirrored; per-session state is evicted on release and bounded; index
  prefixes containing `-v-` or ending in `-v`, URL query strings or fragments,
  and `timeout_ms = 0` are refused. Errors follow the `[serve]` redaction
  rules: an unknown `kind` is not quoted, and `api_key.env` is quoted only
  while it reads as a variable name.
- `lambo recall-index backfill --session <s>`: rebuild one session's recall
  index from the store under the session's lease (#18).
- `GraphStore::holder_derive_source()` (default `HolderDeriveSource::Store`)
  and the `HolderDeriveSource` enum: a store whose checked vector read lags
  the session holder declares where the holder's hybrid derive takes its
  semantic-merge candidates, while recall keeps reading the store (#18 review
  M6, amending #8's single constructor; #60). `HolderDeriveSource::Graph`
  ranks every vector in the holder's in-memory graph; `TieredStore` declares
  it, because its index lags past the flush. `HolderDeriveSource::StoreAndUnflushed`
  asks the store for what the flush has made durable and ranks only the
  holder's unflushed concepts in memory; the Postgres family's `PgStore`
  declares it. The enum is `#[non_exhaustive]`. Additive.
- `GraphStore::backfill_recall_index()` (default `Ok(None)`): the hook the
  backfill verb calls; only a store with a recall tier overrides it. Additive.
- `GraphStore::exact_vector_scan()` (default `false`): an adapter declares its
  checked vector read is an exact cosine scan of every vector it stores, so a
  session holder may answer that read from its graph (#8). `SqliteStore`
  returns `true`. A wrapper around SQLite keeps the database path unless it
  forwards the method, which it should do only when its vector read is plain
  delegation (a wrapper that filters, records or tiers that read must not, or
  a holder would bypass it). Additive: existing adapters are unaffected.
- `lambo serve` logs each of its seven shutdown stages when it starts and
  when it finishes, with the elapsed time (`lambo serve: shutdown stage 3/7
  session_close finished in 12 ms`), then `lambo serve: shutdown finished in
  N ms`. The session close logs its ten steps the same way (`close: step
  4/10 writers_gate started`), and a close abandoned by its timeout or a
  second signal logs, at WARN, the step it was abandoned in. A shutdown that
  stalls now names its stage (#40). The line formats are listed in
  `dev-diary/notes/fix-40-shutdown-stages.md`.
- `lambo erase-session --session <s> --confirm <s>` erases a whole session for
  account deletion (#23): every interaction, concept, vector, edge, synonym,
  reservation, canonization record, durable write intent (their payloads hold
  concept text), published flush stats row and lease refusal, plus the
  `sessions` row with its embedding contract. It prints one JSON report after
  the store committed: counts removed per kind, `already_absent` (nothing was
  left to remove, as on a repeat), and the tombstone's fencing token.
  - Safe to repeat, and one transaction on every store (SQLite, PostgreSQL,
    CockroachDB, memory): a failure part way leaves the session as it was and
    a rerun completes.
  - The session's lease row is replaced by a tombstone rather than deleted.
    Every write to the erased session is refused with a stable "was erased"
    error whatever fencing token it presents, unleased writes and fixture
    seeds included, and recreates nothing; no writer can take the session
    over, and `derive`, `serve` and the other writer verbs refuse to attach.
    Reusing an erased id is a deliberate operator act: an UPDATE that hands
    the row back and keeps its fencing token (statement in the CLI reference),
    never a DELETE.
  - Edges in other sessions that point at the erased session's concepts or
    interactions are removed with it (counted in `edges`).
  - It never preempts a live writer. While a `serve` or writer verb holds the
    session, the erase is refused and names the holder; stop it, then erase. A
    writer whose lease had already lapsed is fenced by its first refused write
    or its next heartbeat, whichever comes first, and winds down; a `serve`
    proxying to the session answers the next call with the erased error and
    exits.
  - An operator verb: the authority is access to the store, never an agent id,
    and there is no MCP tool for it. A `--confirm` that differs from
    `--session` is a usage error and erases nothing.
  - Not reached: backups and snapshots taken before the erase, and the serve
    call ledger file (`--ledger`), which can hold recall queries and truncated
    concept text. The operator's retention policy governs both.
- `GraphStore::erase_session` (with `store::erase`'s `EraseReport`,
  `EraseOutcome` and tombstone rule). The default implementation returns a
  `Capability` error, so a third-party adapter that does not implement it
  fails closed instead of reporting a deletion it did not do. `EraseCounts`
  and `EraseReport` are `#[non_exhaustive]`.
- Decisions for #23: erasure is CLI-only (an MCP or portal surface waits for
  #32's authority design), the tombstone keeps the plain session id, and the
  `serve --ledger` file and backups are operator-owned and not scrubbed.

### Fixed

- `lambo serve` over stdio, and the session endpoint a proxying serve
  forwards to, now cap one request at 4 MiB before parsing it, the same
  ceiling as the HTTP body limit (#101). rmcp reads those transports a line
  at a time with no limit and parses every argument into memory before any
  Lambo check runs, so an oversized `image.data` on `lambo_derive_image` or
  `lambo_recall`, or an oversized `vector.values` or `query_vector.values`,
  was allocated in full, and then again as a JSON value tree, before the
  tool refused it. A longer request line is now discarded unread up to its
  newline and logged at WARN with its size, never its content, and answered
  with a JSON-RPC error, code `-32600`, "request too large (over 4 MiB)",
  keyed to the request's `id` when it can be read from the line's start or
  end (before or after `params`), else to `null`. The connection stays open
  and the session goes on serving. A proxying serve drops such a request
  from its client itself, unforwarded, and answers it the same way.
  Requests of at most 4 MiB reach rmcp byte for byte, so a field over its
  own cap inside one is refused by the tool exactly as before (same
  message, still a tool error), and the tool schemas are unchanged. The
  HTTP service now sets rmcp's body ceiling from Lambo's constant instead
  of inheriting rmcp's default; it was already 4 MiB and answers `413`
  before parsing.

  **Behaviour change for stdio clients that sent requests over 4 MiB.** The
  cap is not above every request the tools' own limits allow: a
  `lambo_derive` with 64 concepts and 256 `parent_of` pairs at 16 KB a
  string is about 9.5 MB, a `lambo_derive_image` with a full image and 256
  pairs about 11.2 MB, and `\uXXXX` escaping makes either up to three times
  larger. Stdio used to accept those; they are now refused with the error
  above, as HTTP already refused them with `413`. Split such a batch into
  calls under 4 MiB. The `parent_of` limit (256 pairs, 512 concepts and
  pairs combined) is now in the MCP reference's limits table.
- On PostgreSQL and CockroachDB, a paraphrase derived seconds after the
  original now merges into it instead of becoming a permanent near-duplicate
  (#60). The database's vector search sees only flushed rows, and the
  write-behind flush can lag by minutes. The process that holds a session
  (`lambo serve`, or an embedded `Memory`) now asks the database for the
  concepts already flushed and compares the new concept against its own
  in-memory copy of the ones it has not flushed yet, then ranks the union by
  exact cosine similarity on its own vectors. A concept stays in that
  unflushed set until the flush that carries it has committed (one the flush
  drops stays in it for good), so nothing is in neither place. The cost stays
  flat in the session's size: the database's indexed search plus about
  0.8 µs per unflushed concept per new concept (about 0.8 ms with 1,000
  unflushed, measured in a release build at 1,024 dimensions), where
  comparing against every concept in memory took 8.5 ms per new concept at
  10,000 concepts, 46 ms at 50,000 and 98 ms at 100,000. Until a session's
  first flush (or after a re-embed, until it is flushed), the database cannot
  answer under the session's embedding contract, so the holder compares
  against every concept it holds; it does the same while about 2,000
  concepts are unflushed (2,048 minus the derive's limit, counting distinct
  concepts and pending deletes rather than writes), the most the database's
  read can over-fetch for. This is the per-purpose split #18 made for
  the Elasticsearch tier: `PgStore` declares
  `HolderDeriveSource::StoreAndUnflushed`. Recall is unchanged and still
  ranks vectors in the database. Merge scores are now exact cosine instead of
  the database's distance (`1 - d` on Postgres, `1 - d²/2` on Cockroach), so
  a pair whose similarity sits right at `semantic_match_threshold` can decide
  differently than before; a flushed concept the Cockroach approximate index
  misses is still missed, as before. The library function
  `graph::hybrid::derive` keeps asking the database only (see its docs).
  A batch the flush drops instead of committing (a dead-lettered constraint
  violation, a degraded session's drop, a lost lease) keeps its concepts in
  the unflushed set until the process restarts, since they never reached
  the database. A degraded session, or one dead-lettered batch of about
  2,000 concepts, therefore derives by comparing against every concept in
  memory for the rest of the process; the holder logs a warning the first
  time that happens.
- A freshly derived text or image concept ranks by query relevance before
  the daemon scores it (#79). Previously its missing daemon score counted as
  0 inside the 0.5/0.5 blend, so a fresh relevant EG2 image (0.3616) ranked
  below older, daemon-scored noise (about 0.655). Now, while a keyword- or
  vector-backed phase-1 concept that query relevance alone would show
  (within `top_k`, or force-included as hot) has no score-table entry, that
  recall scores every hit `w_query × query relevance`, old hits included. A
  fresh concept that could not make the result leaves the blend alone.
  Applying the query score only to the new concept would instead lift fresh
  noise over an established relevant hit with a low daemon score. A
  recent-only hit, a traversal or sibling member, a stale vector id and an
  explicit daemon 0 do not start this mode; `w_query = 0` keeps daemon-only
  ranking. While cold, traversal and sibling members score 0 and rank last,
  and a reservation holder the blend would have shown is kept with its
  warning. The recall's `serve --ledger` line carries a new `cold_start`
  boolean (additive; `v` stays 1).
  The window lasts until the daemon's next completed cycle, not just the
  rescore. On a dogfood-scale fixture graph (6,466 nodes, 11,600 edges,
  release) a cycle takes about 9 ms (rescore about 5 ms, detectors about
  6 ms), a GC sweep about 11 ms more, and an isolated derive is scored in
  6 ms median, 23 ms worst of 60. It persists through sustained write
  bursts: with a derive every 2 ms some concept stayed unscored for the
  whole 3 s burst. Replaying the dogfood ledger, whose write rate is low
  (about 20 derives a day), 1.18% (window up to 50 ms) to 1.55% (1 s) of
  recalls could overlap a window; a write-heavy multi-agent session will
  see more, which `cold_start` now makes measurable. Once the daemon scores
  the concept, the normal blend returns and ranks can change.
  Expected-output change: the assembly test fixture with a deliberately
  unscored phase-1 member (c2) now orders `c1,c2,c5,c3,c4,c6`
  (`.75,.375,.15,0,0,0`) instead of `c1,c2,c5,c6,c4,c3`
  (`.95,.375,.30,.15,.10,.05`): its structural-only members lose their
  daemon share and tie at zero. The blended-formula golden is kept byte for
  byte with an explicit daemon 0 for c2. No fixture file changed, and every
  golden test passes with the cold mode made to panic, so fully scored
  goldens (recall, context and H3 payload goldens) are unchanged.
  `RECENT_SCORE`, merge thresholds and cosine scales are untouched (#87).
- A daemon rescore that panicked no longer leaves the score table stale
  until the next write: the cycle marks its epoch done only after
  publishing, so the next cycle retries. The rescore is contained on its
  own, so one that panics every cycle no longer stops detection, the hot
  list and GC; it logs one warning per epoch (#79 review).
- `graded_similarity_ranks_by_cosine_not_recency_on_sqlite` no longer fails
  intermittently. The daemon's recency is a concept's millisecond position
  in the session's wall-clock span, and the fixture's session lasts about
  2 ms. A scheduler stall before the 0.3 look's derive gave it recency near
  1 against 0 for the 0.5 look, enough under the 0.5/0.5 blend to flip
  them. The graded looks are now derived worst first, so recency can only
  widen the cosine order; a stalled variant pins it. The holder and tier
  tests that share the fixture had the same latent flake. #79's rule is not
  the fix: every look is daemon-scored when the test reads.

- A `bge_m3` or `embeddinggemma2` input longer than the llama-server's
  physical batch is now a content refusal, settled as failed with a hint
  naming `--ubatch-size`. llama-server answers it with HTTP 500 ("increase
  the physical batch size"), which Lambo read as a busy server, so such a
  concept was retried forever.
- The `embeddinggemma2` embedder no longer drops its `/props` and image
  budget checks on a 503 "busy" (each retried image cost an extra `/props`
  GET and reference embed); only no answer at all or a 503 "Loading model"
  reads as a restart. A server that refuses Lambo's reference image is
  reported as a server problem, not a decode failure of the user's image.
  `/props` strings (file name, `model_ftype`) in messages and logs are cut
  to 128 printable ASCII characters.
- A text recall whose vector read fails (a backend error, a timeout, a tier
  whose durable fallback failed too) still answers from its keyword and
  recent legs, but no longer silently: the result carries a
  `vector_degraded` annotation and the same line in `warnings`, so
  `Memory::recall`, `lambo_recall` and `lambo recall` all say the vector leg
  was skipped. The line names no backend detail (that is logged). The
  embedding-contract race's `vector_degraded` line (E2E-6) now reaches
  `warnings` too, where before only the CLI and the portal showed it.
- `lambo serve-web` validated no `Host` header (#4 PR 2), so with no
  credential configured any web page the local user visited could rebind its
  own name to `127.0.0.1` and read every served session same-origin. Without
  a credential it now answers only `localhost`, `127.0.0.1`, `[::1]` (any port) and the
  allowed hosts, and anything else, including a missing or malformed `Host`
  (user info, an empty or non-numeric port) and a request with two `Host`
  headers, gets one fixed `403` before any other check. With any credential
  configured (a token, a `[[web.credential]]` or an inherited one) any
  `Host` is accepted, as `lambo serve` does (a rebound page cannot present
  a token).
- The multi-session refusal poller kept a ledger cursor only for pinned
  sessions, so an on-demand session's cursor would have been rebuilt every
  round and re-booked the last `LEASE_TTL` of lease refusals each time (#32,
  sixth part; caught before release). A cursor is now kept while its
  session is pinned or attached, and for `LEASE_TTL` after an on-demand
  session is detached (at most 1,024 such), so a reattach inside that
  window does not book its refusals again either.
- A close on a handle whose lease was lost aborted its flush,
  canonization and daemon tasks without waiting for them to stop, so a
  flush already past its store commit could still mirror into the recall
  index after the close returned. They are now aborted and joined (#32,
  seventh part).
- `lambo serve-web`'s `/api/pulse`, polled every 1.5 s by every open tab,
  loaded the whole session twice: once for the event feed and again for the
  counts (#4 PR 1). `/api/stats` did the same. Each now costs one load (and
  with the shared view, none inside the TTL), and the feed and the counts in
  one response always come from the same snapshot.
- A `lambo serve` with a token bound beyond loopback answered `403` to every
  MCP request, because rmcp's default `Host` allow-list admits only
  `localhost`, `127.0.0.1` and `::1` (#32, fifth part). A serve that requires
  a token now accepts any `Host`; a loopback serve with no credential keeps
  the allow-list as DNS-rebinding protection.
- An MCP session id answered any credential that presented it; it now
  answers only the credential that opened it, and another gets rmcp's
  unknown-session answer (#32, fifth part).
- `lambo_stats` and the ledger's stats heartbeat no longer under-report a writer's not-yet-durable mutations. The flush task
  drained the graph's log into its pending batch and updated its depth only
  after releasing the graph lock, so `log_depth + flush_depth` could read 0
  for a session that was still dirty. The depth is now published under the
  same write lock as the drain, and `Memory::stats` reads both under the graph
  lock. Observability only: nothing was ever lost.
- The ledger's applied `completion` lines (`applied` and
  `applied_after_restart`) now carry `semantic_merged`, `reinforced`, `edges`
  and `embedded` beside `created_count` / `matched_count` (#12), so the
  metric-2 facts and embedding coverage outlive the 300 s receipt. Additive:
  `v` and the existing keys are unchanged, and each new key is present only
  for the write kind that has it.

- The write queue's startup probe no longer reports `unmeasured` when the
  embedder's first call is slow (#11). Its discarded warm-up embed shared
  the 5 s budget of the timed legs, and the candle Metal BGE-M3's first
  embed on a cold page cache outran it, so the session never had a probe
  figure to compare its writes against. The warm-up now has its own 30 s
  bound.
- A reported write-queue rate no longer exceeds 1024 items/s (#11). The cap
  was documented but applied only to a zero time, so a fixture embedder's
  probe published about 200,000 items/s.
- A `lambo serve` shutdown is now bounded even when its own timers cannot
  fire (#40). Every shutdown bound is a timer inside the server's async
  runtime, and a wedged runtime (every worker thread blocked, or the thread
  driving the server blocked) fires none of them, so the process logs
  nothing more and waits for its supervisor's kill. The live writer's stall
  in #40 is consistent with that (it was not reproduced). A watchdog thread
  outside the runtime now warns when a stage outlives its own bound by more than 1 s,
  and 20 s after the shutdown began it logs the stalled stage and aborts the
  process. On macOS the abort leaves a crash report with every thread's stack
  in `~/Library/Logs/DiagnosticReports/`. A healthy shutdown takes at most
  18.5 s, so the watchdog never fires on one. **Operator action:** a
  supervisor's kill timeout should exceed 20 s so the watchdog acts first; 30
  s is recommended (launchd `ExitTimeOut`, whose default is 20 s; systemd
  `TimeoutStopSec`). Pre-existing.
- A clean lease release no longer resets a session's fencing token (single
  writer, #1; found by the #23 review). A release deleted the
  `session_leases` row, so the next acquire minted token 1 again, and a writer
  whose lease had lapsed and been taken over could write over the next holder
  once the taker closed cleanly. A release now expires the row (holder
  `lambo:released`) and keeps its token, so tokens only go up for the life of
  a session id, on SQLite, PostgreSQL, CockroachDB and the in-memory store. A
  session that was ever leased therefore refuses unleased writes, as it
  already did while an expired row was present. The documented operator
  override for a wedged holder (`store::lease::OPERATOR_OVERRIDE`) is now the
  matching UPDATE; never delete a lease row.
- A `Memory::close()` cancelled while it was stopping the background write
  queue (for example by a caller's timeout) no longer leaves lane workers or
  the durable-intent replay running. Every worker is aborted before any is
  joined, and handles not yet joined are kept, so a retried `close()` waits for
  them before it drains the log (#27). That retried `close()` also no longer
  waits the full 2 s write-queue drain budget for workers the cancelled call
  had already aborted; it goes straight to joining them. No write was lost
  before: each job is a durable intent the next serve replays.
- `lambo serve-web` checks its bearer token with the same constant-time
  comparison as `lambo serve --transport http` (#28). Its own copy looped
  over the configured token, so the time a check took tracked the secret's
  length; the shared one loops over the presented value, which the caller
  already knows. Which tokens are accepted is unchanged. Pre-existing.
- A holder's shutdown now ends the sessions of clients attached through the
  session endpoint (proxies) once the session is closed, and waits for them,
  for up to 3 s (#28). They used to stay connected until the process exited,
  so a call arriving after the close was refused against a closed session and
  could still write to the call ledger as it drained. A proxy sees the same
  dropped connection a holder exit gives it, a little earlier. A shutdown with
  proxies attached can take up to 3 s longer, after the tail is durable and
  the lease released, never before. Pre-existing.
- Concepts created as `parent_of` ends are embedded (issue #16 §2). Hybrid
  `derive` created an end named only in `parent_of` with no vector, so it was
  invisible to recall's vector leg until `re-embed --missing-only` backfilled
  it, and the receipt read "2 created (1 embedded)". Ends the call creates are
  now embedded with the same context, deadline, failure rule and contract
  stamp as its concepts, and counted in `embedded`. They are not matched
  against existing concepts. A store that advertises vector search but refuses
  the checked lookup leaves them keyword-only, whatever else the call carries.
- Hybrid `record_action` no longer embeds on a store without vector search,
  matching `derive`: nothing is embedded or stamped, and the receipt says
  `(0 embedded)`. Pre-existing.
- The hybrid embedding-context cap counts the real framing bytes (5 with an
  origin, 9 without, not 3), so a context can no longer exceed 16 KiB by up to
  6 bytes. Pre-existing.
- The in-memory graph's write gate refuses a concept whose id names an
  existing interaction, before anything is written or logged (#50). It used to
  overwrite the interaction, and the flush then stored both rows under one id,
  leaving a session that could no longer be loaded. No shipped caller passed
  such an id. An interaction whose id names a concept is now refused with a
  message that says so, instead of one that read as internal corruption.
  Pre-existing.
- Loading a session snapshot refuses a node id that appears twice (two
  concepts, two interactions, or a concept reusing an interaction's id) instead
  of keeping the last concept or failing on an unrelated-looking check (#50).
  No store writes such a snapshot. Pre-existing.

## 0.3.0 (2026-10-07)

### Breaking

- No Windows binary in 0.3.0. The shared session endpoint added for multiple
  clients uses Unix sockets and Unix file metadata with no platform gate, so the
  crate does not compile for `x86_64-pc-windows-msvc`, and regular CI never built
  for Windows to catch it. The release matrix drops the Windows entry and
  `install.sh` points Windows users at v0.2.2, the last release with a Windows
  build. Restoring Windows is #39.

- GC sweep accounting is durable and GC's step-2 cut changed (issue #29). Public
  structs gained fields, so struct-literal construction breaks where
  `..Default::default()` is not used: `MutationBatch::gc_mark` and
  `GraphSnapshot::gc_mark` (new `lambo::types::GcMark`; serde-defaulted and
  omitted when unset, so pre-#29 JSON round-trips unchanged),
  `Config::{gc_max_interval, gc_idle_floor}`,
  `DaemonConfig::{gc_max_interval_secs, gc_idle_floor}`,
  `CycleParams::{gc_max_interval, gc_idle_floor}`,
  `GcParams::{recency_window, max_collect_fraction, min_collect_cap}` and
  `GcOutcome::{collection_cap, collections_deferred, deferred, resources_spared_by_dependents, trigger}`.
  **Operator action:** the schema gains `sessions.last_gc_epoch` and
  `sessions.last_gc_at` on all three dialects, so an already-provisioned
  store (SQLite included: the attach preflight reads the DDL) refuses to
  attach until `lambo provision` re-runs; provisioning adds the columns in
  place (guarded `ALTER`, no data touched). If the rigs have not yet been re-provisioned for
  #17's `mutation_epoch`, one re-provision covers both. Existing rows backfill
  watermark 0 and no sweep time: the first writer to attach starts the
  `gc_max_interval` clock rather than sweeping, so the first timed sweep comes
  a day (and at least 100 mutations) after the upgrade.

- `lambo::Mutation` gained a `RecordAccess` variant (issue #30: the narrow,
  monotonic access-column update), so an exhaustive `match` on it no longer
  compiles; `lambo::store::batch::FlushStep` gained `Accesses` and
  `BulkLimits` gained an `accesses` field for the same reason. Not a wire or
  persisted format: mutations are applied in-process, and the only serialized
  form (the fixtures' JSON loader) gains an additive `record_access` tag.

- `lambo::canon::gate_progress` takes a `PromotionPolicy` argument (fourth
  position, before `min_edge_age`). It decides whether the four store-evidence
  gates are measured at all, so the caller cannot be trusted to check it: under
  `Solo` none of `gc_survived`, blast radius, distinct interactions or coverage
  participates in promotion, and shipping them anyway put "0 of 4 gates met"
  beside a concept due to go Canonical on the next cycle.
- `GateProgress`'s four gate fields moved into a new `SwarmGates` struct behind
  `GateProgress::gates: Option<SwarmGates>`, absent under `Solo`. Rust field
  access moves one level down, but the **move itself** changes no JSON: the
  holder is `#[serde(flatten)]`ed, so under `Swarm` the four keys serialize in
  the same place, with the same names, in the same order as before. The object
  around them is not unchanged — it gained `policy`, and under `Solo` the four
  keys are absent — so a client that only reads the four gates under `Swarm`
  needs no change, while one that enumerates the object's keys does. The
  cooldown fields stayed put deliberately: the re-promotion cooldown is
  policy-independent, and this struct is its only carrier.
- `GateProgress::met_count` returns `Option<usize>` rather than `usize`: `None`
  when the live policy does not read the four gates, so a caller cannot render
  "0 of 4" for a concept whose policy has no such count.
- `LamboFile` gained a public `promotion_policy: Option<PromotionPolicy>`
  field. The struct has all-public fields and no `#[non_exhaustive]`, so every
  external struct-literal construction and exhaustive destructuring of it
  breaks. `..Default::default()` does not.

- `lambo serve` exits **0**, not 1, when a client disconnects before completing
  the MCP `initialize` handshake. That is `rmcp`'s stream-ended case — on stdio,
  EOF on stdin — and reporting it as a config error called an ordinary
  lifecycle event a misconfiguration: the session had already closed and
  flushed cleanly, and the failing status was appended to a successful
  shutdown. It also split the contract across the handshake boundary, since an
  EOF one frame *later* already exited 0. Anything scripting `lambo serve` that
  treated a pre-handshake hangup as a failure will now see success. Every other
  handshake error stays fatal: a client opening with the wrong frame, a
  protocol violation, or a transport fault is still a non-zero exit.

- `store.kind = "postgres"` and `"pg"` now select `StoreKind::Postgres`, not
  CockroachDB. `"cockroach"` and `"crdb"` remain Cockroach. A leftover
  `kind = "postgres"` pointed at a Cockroach cluster fails at provision
  (`CREATE EXTENSION vector`) or first vector query (`<=>`) rather than
  silently ranking with Cockroach SQL.

- `ActionOutcome` gained a public `embedded: usize` field — how many of
  `created` were written with a vector. The struct has all-public fields and no
  `#[non_exhaustive]`, so external struct-literal construction and exhaustive
  destructuring of it break; field access and `..` patterns do not. It is
  reported rather than inferred so that "embedded nothing because it created
  nothing" is distinguishable from "created concepts and left them unembedded",
  which is the exact distinction whose absence hid the defect below.

- Public types gained fields since v0.2.2. None of these structs is
  `#[non_exhaustive]`, so external struct-literal construction and exhaustive
  destructuring break; field access and `..Default::default()` / `..` patterns
  do not. Serialized forms stay compatible: every new serde field is
  `#[serde(default)]`, so JSON written by v0.2.2 still loads.
  - Graph types: `Concept::human_confirmed: i32`;
    `event_time: Option<DateTime<Utc>>` on `Interaction`, `Edge` and
    `lambo::graph::Action` (omitted from JSON when unset);
    `GraphSnapshot::{mutation_epoch, write_intents}`;
    `MutationBatch::mutation_epoch`.
  - Outcomes and stats: `DeriveOutcome::embedded`,
    `MemoryStats::embedded_concepts`, `CanonicalMemory::canonical_key`.
  - Configuration: `Config::promotion_policy`, `EvalParams::promotion_policy`,
    `StoreConfig::vector_dim`, `ServeOptions::{ledger, ledger_heartbeat}`, and
    eleven `EmbedderConfig` fields for the candle and Gemini embedders
    (`device`, `repo`, `revision`, `weights_dir`, `weights_file`, `offline`,
    `keep_warm_secs`, `gemini_project`, `gemini_location`, `gemini_model`,
    `gemini_credentials`).
  - Leases and loading: `LeaseHolder::endpoint`, `LeaseInfo::endpoint`,
    `LoadedSession::write_intents`.
  - MCP parameter structs: `DeriveParams::event_time`,
    `RecordActionParams::event_time`, `StatsParams::{receipt, wait_ms}`; the
    CLI's `re_embed::Args` gained `allow_embedding_mismatch` and
    `missing_only`.

  **Operator action:** the new `event_time` (interactions, edges),
  `human_confirmed` (concepts) and `session_leases.endpoint` columns are added
  in place by `lambo provision` (guarded `ALTER`, no data touched); the same
  re-provision #17 and #29 call for covers them.
- Public enums gained variants, and none is `#[non_exhaustive]`, so an
  exhaustive `match` on them no longer compiles: `StoreKind::Postgres`,
  `EmbedderKind::{Candle, Gemini}`, `LamboError::{EmbedUnavailable, SoftLock}`,
  `Mutation::{PutWriteIntent, ConsumeWriteIntent}` and
  `lambo::store::batch::FlushStep::PutIntents`.
- `lambo::cli::provision::run` takes a third argument, `dsn: Option<&str>`
  (the resolved `store.dsn`; only the Cockroach arm reads it).
  `lambo::store::cockroach::CockroachStore` is now a type alias for
  `PgStore<CockroachDialect>`; its `new` and `seed` keep their signatures.
- Public constants changed value. `lambo::recall::candidates::RECENT_SCORE`
  dropped from `0.5` to `0.35`, a **behaviour change**: concepts that enter
  recall only through the recent-interaction leg now score 0.35, so they rank
  below any genuine BM25 or vector hit (BGE-M3 calibration put the lowest
  durable true semantic hit at 0.3991) instead of above many of them, and
  recall scores a client sees for those concepts drop accordingly (see
  `evidence/mooshik-g-recall-calibration/`). The bind-parameter counts in
  `lambo::store::batch` grew with the new columns: `CONCEPT_COLUMNS` 16 to 17,
  `INTERACTION_COLUMNS` 6 to 7, `EDGE_COLUMNS` 9 to 10.

### Added

- `lambo_stats` (and the `serve --ledger` heartbeat) carries a `gc` object:
  the session's durable sweep mark (`last_gc_at`, `last_gc_epoch`) and the
  last sweep this server ran (`trigger`, `collected`, `deferred`,
  `collection_cap`, `cap_bound`, `resources_spared_by_dependents`,
  `survivors_deferred`; `null` until one runs), plus a `gc:` line on the text
  summary. Additive: no existing key changed. Library: `Memory::gc_stats`,
  `GcStats`, `GcSweepSummary` (issue #29).
- `SessionEndpoint::resolve_in` is now public: `resolve` with the endpoint
  directory supplied, for deriving the address a holder would bind in a
  directory other than this process's own.
- `promotion_policy` as a top-level `lambo.toml` key with a
  `LAMBO_PROMOTION_POLICY` environment override (non-empty env wins; an empty
  value is unset, as everywhere else Lambo overlays the environment). It
  threads onto the `Config` that `Memory::builder().config(..)` reads, so a
  `lambo serve` process selects a canonization policy without a code change.
  The default stays `Swarm`, and an unset key does not move existing
  behaviour. Both surfaces parse through `PromotionPolicy::from_str` —
  trimmed and case-insensitive, like `store.kind` and `embedder.kind` — and an
  unrecognised value is a hard startup error naming both what was written and
  the valid set, never a silent fallback to the default. There is no
  `serve --promotion-policy` flag: no `serve` flag in Lambo duplicates a
  `lambo.toml` key.
- `PromotionPolicy::ALL`, `FromStr` and `Display`. Every refusal message
  enumerates `ALL` and `from_str` searches it, so a variant missing from it
  would be unselectable from both `lambo.toml` and `LAMBO_PROMOTION_POLICY`
  while every refusal kept naming only the others. `ALL` and the enum are
  therefore generated from one variant list, length included, which makes that
  state unrepresentable rather than merely tested — a `PromotionPolicy` variant
  cannot be declared anywhere else.
- `promotion_policy` on the `lambo_stats` payload, its text summary, and the
  `serve --ledger` heartbeat. The payload and the heartbeat come from one
  shared builder (`stats_json`) and so cannot disagree; the text summary is a
  separate `format!` over the same live `Config`, so all three report the same
  value but only two of them are structurally prevented from drifting.
  Read from the live `Config`, so it reports the value that won —
  file, environment, or default. The cycle counters cannot answer the question:
  `canonization_cycles` climbs identically under both policies, and
  `canonical_count` staying at zero is both the normal reading for a young
  `Swarm` session and the whole symptom of a `Solo` selection that did not
  take.
- `/api/inspect` names its own resolution in `promotion_policy` (always
  present, including on a miss) and labels every absent gate block in
  `gate_progress_omitted` (`already_canonical` or `unavailable`). `serve-web`
  is a lease-free reader that resolves its own config, so a gate count with no
  policy beside it was unattributable; the two omission causes used to be one
  indistinguishable `null`. The page acts on the difference rather than merely
  receiving it: `already_canonical` draws nothing, and `unavailable` says the
  checks could not be read — a failed store query no longer renders identically
  to a promoted fact.
- `lambo::RESOLVE_ENV_VARS`: **every** environment variable a Level B resolve
  reads, in one place, for tests and harnesses that need the config to be
  exactly the `--config` file they wrote. Replaces five hand-maintained copies
  of the same list, which had already fallen out of step — and completes it:
  the five copies all named nine variables, omitting `LAMBO_POSTGRES_DSN` and
  the five (`LAMBO_EMBED_DEVICE`, `LAMBO_GEMINI_*`) that `EmbedderConfig`
  overlays regardless of `embedder.kind`. A harness that cleared the old list,
  wrote `store.kind = "postgres"` with no `dsn`, and ran `lambo provision` took
  its DSN from the ambient shell.
  "Every variable a resolve reads" also covers what `build_store` and
  `build_embedder` read after the file overlay, which the first version of this
  list did not: `GCP_LAMBO_CREDENTIALS`, `GOOGLE_APPLICATION_CREDENTIALS` (both
  through `gcp_auth::credentials_path_from_env`, called eagerly by the Gemini
  embedder build and by `PgStore::new` under the IAM opt-in) and
  `LAMBO_POSTGRES_IAM`. Those three pick an *identity* rather than a database:
  a harness that cleared the old list and built a Gemini embedder with no
  `gemini_credentials` authenticated as whatever service account the ambient
  shell named, and billed Vertex calls to it. `LAMBO_VECTOR_BEAM_SIZE` is
  excluded on purpose — it is read when the pool is first used, not during the
  resolve.
- `lambo::store::POSTGRES_IAM_ENV`, the `LAMBO_POSTGRES_IAM` name as a public
  const, declared unconditionally so `RESOLVE_ENV_VARS` can name it in every
  feature row. Pinned equal to the `store-postgres` const `PgStore::new`
  actually reads.

- `StoreKind::Postgres` and Cargo feature `store-postgres` (same sqlx postgres
  driver as `store-cockroach`; no second driver). B2 lands templated-width
  pgvector DDL with an hnsw index from init. Dimensions above 2000 are
  refused at init, naming the pgvector hnsw ceiling and the unimplemented
  `halfvec` hatch. `lambo provision` for `kind = "postgres"` runs
  `init_schema` (not `scripts/provision.sh`).
- DSN identity normalisation beside `store_identity`: two spellings of one
  database (`postgres://u@host/db` and `postgres://u@host:5432/db`) derive one
  session endpoint. Password is stripped from the identity so it never reaches
  the filesystem or the lease row.
- `store_is_shareable(Postgres) = true`: a networked store another process can
  open, ruled in the exhaustive match, not defaulted.
- Postgres ranking: pgvector `<=>` (cosine distance) converts with `1 - d`.
  Cockroach stays `<->` L2 and `1 - d^2/2`. Copying either formula onto the
  other dialect is pinned to fail.

- Cargo feature `embed-gemini`: Vertex `gemini-embedding-001` embeddings, with a
  dim guard for the 768 / 1536 / 3072 the model truncates to.
- `src/gcp_auth.rs`: one Google OAuth path for every adapter that authenticates
  as a Google principal, gated `embed-gemini` OR `store-postgres`. Handles both
  credential kinds (service-account key by `jwt-bearer`, authorized-user ADC by
  `refresh_token`) and asks for the caller's own scope on both grants (the signed
  `scope` claim on one, the `scope` form field on the other), so a Cloud SQL login
  and a Vertex call share an identity without sharing authority. On an ADC the ask
  can only narrow what `gcloud auth application-default login` granted; asking for
  more is refused as `invalid_scope` by the token endpoint rather than later by the
  database.
- The Gemini embedder resolves credentials from `GCP_LAMBO_CREDENTIALS` before
  `GOOGLE_APPLICATION_CREDENTIALS`, the chain the Postgres store already used, so
  one exported variable names one identity for both.
- Cloud SQL IAM database authentication for `store-postgres`, opted into with
  `LAMBO_POSTGRES_IAM` set to a **non-empty** value (empty is the ordinary password
  path, as everywhere else lambo overlays the environment): the connection password
  is an OAuth token minted from the shared credential file, and the pool is rebuilt
  when that token expires. The opt-in is PostgreSQL only
  (`Dialect::SUPPORTS_CLOUD_SQL_IAM_AUTH`), so a build carrying both adapters never
  hands a Cloud SQL token to a Cockroach cluster.
- `scripts/cloudsql-allowlist.sh`: add the running host's egress IP to a Cloud SQL
  instance's authorized networks, idempotently, preserving entries it did not add.
- Released binaries (`ship`) now carry `store-postgres` and `embed-gemini`, so a
  `lambo.toml` naming `postgres` or `gemini` runs on a prebuilt binary.
- `lambo serve` embedder keep-warm (issue #13): a holder-side task embeds one
  short fixed probe every interval and discards the vector, so an idle writer's
  recently touched model weights are more likely to still be resident when the
  next call lands, which reduces how often a call pays the swap-in. It does not
  guarantee residency under heavy memory pressure. Set with `[embedder]
  keep_warm_secs` / `LAMBO_EMBED_KEEP_WARM_SECS`: omitted means auto, `0` is
  off, `N` is every N seconds for any embedder kind. **Behaviour change for
  candle on Metal:** auto is on there (every 10 s), because those weights sit in
  unified memory the macOS pager compresses. Everything else defaults off: CUDA
  weights live in VRAM, the fixture has no weights, and `bge_m3`/`gemini` hold
  their weights in another process. A touch writes nothing (no store I/O, graph
  mutation, ledger line or recall-cache entry), is not part of the embedding
  contract, and first fires one interval after startup, so the handshake gains
  no work. Proxies and one-shot CLI commands never run it.
- A Metal release asset, `lambo-<version>-macos-arm64-metal` (with its
  `.sha256`): the `ship` adapter set plus `embed-candle-metal`, so an Apple
  silicon machine can run the in-process candle embedder (`[embedder]
  kind = "candle"`, `device = "metal"`) from a published binary instead of a
  hand build. It links only macOS system frameworks and `/usr/lib` libraries,
  which the release workflow asserts. The stock `ship` assets are unchanged and
  still carry no candle.
- `install.sh` takes `LAMBO_FLAVOR=metal` to install that asset, with the same
  SHA-256 verification. It is refused with an error on anything but macOS arm64;
  unset, the script installs the stock build as before. `LAMBO_DRY_RUN` set to
  any non-empty value other than `0` (`1`, `yes`, `true`, ...) prints the asset
  and URLs it would fetch, then exits without downloading; unset, empty or `0`
  installs.
- The release workflow can be run by hand (`workflow_dispatch`, `dry_run`
  default true) to build, parity-test and checksum every asset as workflow
  artifacts without tagging. A dispatch never publishes a GitHub release or a
  crate: those jobs run only on a `v*` tag push, and a dispatch with
  `dry_run=false` is refused. Release binaries are now built with
  `LAMBO_GIT_SHA`, so the `serve --ledger` heartbeat names the release commit
  instead of `unknown`.

- Historical event time on writes (D1/D2). `lambo_derive` and
  `lambo_record_action` accept an optional `event_time`, an RFC3339
  about-time such as a commit or document date, stored on the interaction and
  its edges beside the server-stamped `created_at`. Omit it for a live fact,
  which is about now. Every other timestamp argument is still refused by
  name, so `created_at` stays server-owned.

### Fixed

- SQLite's blast radius now counts only structural dependents (#35). A concept
  the focus reached through any edge, including `CoOccurrence`, `Semantic` and
  `Temporal`, counted as a dependent, while Postgres, Cockroach, the in-memory
  store and the graph count `Dependency`, `Causal` and `Hierarchical` edges
  only. On a 3,370-concept dogfood store 673 concepts got a different value and
  14 cleared Stage 3's `blast_radius > 5` bar through non-structural edges
  alone. Stage 3 and budget demotion recompute blast radius when they evaluate a
  concept, so new decisions use the corrected value; values already stored on
  concepts and in canonization events keep the old count until that concept is
  evaluated again.

- A `lambo serve` that loses the election and proxies to the session holder no
  longer keeps its resolved embedder alive. It used to hold the full model for
  as long as its client stayed attached (with candle on Metal, ~1.1 GB of
  weights plus the coalescer threads) without ever embedding; the backends are
  now released before the proxy starts. The model is still loaded while the
  backends resolve and through the election wait, until the role is known
  (tracked in #31). Pre-existing, found in the issue #13 review.
- GC sweeps at human pace without deleting reasoning by session age
  (issue #29). Behaviour changes, each deliberate:
  - **A restart no longer sweeps by itself.** GC's watermark was per-process state starting
    at 0, so once a session passed `gc_interval` lifetime mutations every writer
    restart swept once and bumped every `gc_survived` — three restarts reached
    Stage 1's floor with no new information. The watermark and the time of the
    last sweep now persist with the session beside `mutation_epoch` (stamped on
    every flushed batch, merged monotonically in the flush transaction,
    resumed on load), so a restarted writer sweeps only if a sweep was already
    due by the stored mark: a previously swept session that took at least
    `gc_idle_floor` mutations and was down past `gc_max_interval` sweeps once
    on its first cycle back, which is intended. A never-swept session anchors
    its clock on first attach instead of time-sweeping, and keeps #17's single
    catch-up sweep on the mutation count. The anchor only delays the timed
    trigger: a never-swept session counts its whole lifetime toward
    `gc_idle_floor`, so one with at least that many mutations sweeps once,
    `gc_max_interval_secs` after the anchor, even if idle since (a deliberate
    catch-up; afterwards the mark is set and idle sessions never sweep).
  - **Timed sweeps.** A session also sweeps once `[daemon] gc_max_interval_secs`
    (default 86 400) has passed since its last sweep, if it took at least
    `[daemon] gc_idle_floor` (default 100) mutations since. Idle sessions never
    sweep on time; a writer down for N days sweeps once. The 10 000-mutation
    trigger and every promotion threshold are unchanged.
  - **Step 2 protects reasoning.** Logic, Constraint and Observation are
    exempt from the score cut (orphans and disconnected components are still
    collected; the exemption is `ConceptType::exempt_from_gc_score_cut`, an
    exhaustive table), so the cut removes only Entities and isolated
    Resources. GC's eviction recency is now time since last touch over a fixed
    365-day window (Lambo is long-term memory; what the cut can still reach is
    mostly long-tail pointers, about 4% of the rig's store) instead of
    position in the session span; recall ranking and canonization
    still use the span-relative score. Collections per sweep are capped at
    `max(32, 5%)` of unprotected concepts; a sweep that hits the cap logs a
    warning and reports `GcOutcome::collections_deferred`.
  - **Resources with dependents are kept (operator decision).** The score cut
    no longer collects a Resource that another concept has a `Dependency`,
    `Causal` or `Hierarchical` edge into (what `record_action` writes into the
    things an action depends on, produces or modifies), nor one whose blast
    radius is non-zero (the action node that is the only structural source of
    something). Isolated, untouched Resources still age out over the 365-day
    window. Reported as `GcOutcome::resources_spared_by_dependents`.
  - **An access can only help in GC's cut.** GC no longer switches the whole
    session to the full composite once any concept has been read (ALGO-1's
    switch, which made every unread concept ~20% easier to collect: one access
    on one concept took the rig's first sweep from 159 to 412). Each concept
    scores the live-dimension score plus its own frequency term
    (`score::score_live_plus_frequency`). Recall ranking, the daemon's score
    table and canonization are unchanged.
  - **A future sweep time is re-anchored.** A forward wall-clock jump used to
    persist a future `last_gc_at` that the monotonic merge kept after the
    clock was corrected, switching the time trigger off until real time caught
    up. A stored time more than 5 minutes ahead of `now` is now re-anchored at
    `now`; the regression persists through a flagged path in the session-row
    upsert (`GcMark::last_gc_at_reset`, never stored). `last_gc_epoch` stays
    strictly monotonic.
  - **Repeated clock jumps persist.** A second forward jump and correction in
    one process now re-anchors in the store too; the writer-side reset flag is
    never carried in snapshots.
  - **Sweeps are cheap.** Scoring no longer scans every interaction per
    concept, so a sweep is linear in the store instead of concepts times
    interactions: on the rig copy the first sweep went from 67.5 ms to 9.0 ms
    under the write lock, with identical results.
  - `lambo_stats` reads GC stats once per answer, and
    `resources_spared_by_dependents` no longer counts a Resource that step 3
    collected as disconnected.
  - On a copy of the Metal rig store the first sweep now collects nothing
    (was 537, including 42 Logic and 324 Observations), and collection no
    longer grows with session span alone. Untouched sparse concepts still age
    out once older than the window: an untouched store converges on 130
    collections (Resource 121, Entity 9; 3.9% of the store), with 884 under-bar
    Resources kept by the dependents rule and no change in the concepts above
    Stage 3's blast-radius bar (41 before and after). One access never adds a
    collection. Details in `dev-diary/notes/gc-time-bound-29.md`.
- `access_count` and `last_accessed` are now written (issue #30). No path ever
  wrote them, so the spec §9 `frequency` dimension was dead: GC's step 2 ran on
  the renormalized three-dimension score (ALGO-1) and could not see what agents
  actually recall (all 3,370 concepts on the Metal rig read 0). Every concept
  hit a writer's recall returns now counts once per recall — cache-served or
  not, inside the token budget or not — and a resolved `lambo_inspect` counts
  its focus concept (not the neighbourhood; a refusal counts nothing). Reads
  note hits in a leaf-locked ledger; the daemon applies them in RAM once per
  tick and marks the concepts access-dirty; the write-behind flush persists
  them as one narrow, monotonic update per concept (`access_count` and
  `last_accessed` only, `max` of stored and new — no full-row rewrite, no
  embedding, no vector-index touch; idempotent on replay, fenced like every
  write) through the existing columns (no schema change, no re-provision), and
  `close` takes the remainder, so the counts survive a writer restart. A
  recall that finishes after `close` took the ledger is not counted. Reads
  never grow the mutation log: during a store outage the flush holds at most
  one access update per concept (and never more than half of
  `backend_log_max`), so read traffic alone cannot degrade a session to
  `durability="none"`, and the latest counts land once the store recovers.
  That held drain can occupy up to half of the degrade budget, so while reads
  are active during an outage, writes alone degrade the session at about half
  the configured bound.
  Accesses do **not** advance the mutation epoch, so GC's `gc_interval`
  trigger, the recall cache and hybrid replanning are unaffected and a
  read-heavy session does not sweep on reads. Reader processes (`lambo
  recall`, `serve-web`) count nothing; a proxied call counts once, in the
  holder. Operator-visible effects: recency counts a read as a touch (the later
  of `created_at` and `last_accessed`); **a read-only session no longer goes
  Stale** — reads are activity, by decision (the staleness detector already
  read `last_accessed`; it now has something to read); the daemon score
  table, which recall ranks with, picks up new frequency only at the next real
  write (it rescores on epoch change; an accepted lag: ranking between two
  writes stays stable and cacheable); `lambo_saints` reports non-zero `access_count`. GC's
  score cut adds each concept's own frequency term on top of the live-dimension
  score (#29's additive rule), so an access can only raise that concept's score
  and never makes another concept easier to collect; accesses never advance the
  epoch, so they cannot reach `gc_interval` or `gc_idle_floor`, and a read
  postpones GC's 365-day recency cut for the recalled concept (see
  `dev-diary/notes/issue-30-access-count.md`). Recall ranking, the score table
  and canonization Stage 1 have no such switch: an access moves only the
  accessed concept's score.
- Canonization now fires in long-running low-write deployments: the mutation
  epoch the GC interval measures persists with the session instead of resetting
  on every writer start (issue #17). Thirteen days of dogfooding produced zero
  consolidations because the chain was closed by construction — Stage 1 needs
  `gc_survived >= 3`, survivor bumps come only from GC sweeps, GC ran only
  every 10,000 **in-process** mutations, and the epoch restarted at 0 each
  start, so a single supervised writer never crossed even one sweep. The fix
  is the accounting, not the predicates: `Graph::drain_log` stamps each flushed
  batch with the absolute epoch, every store upserts
  `sessions.mutation_epoch = MAX/GREATEST(existing, stamped)` inside the flush
  transaction (replays converge; a crash cannot separate the counter from the
  content it counts), `load_session` returns it, and `Graph::from_snapshot`
  resumes it. The daemon is unchanged; a writer attaching to a session whose
  resumed epoch already exceeds the interval sweeps once immediately (a
  catch-up, not new collection criteria), then the normal cadence holds.
  Operator-visible: the schema gains `sessions.mutation_epoch` on all three
  dialects, so an already-provisioned store refuses to attach until
  re-provisioned (`lambo provision`); existing rows backfill to 0 and
  accumulate forward — past mutations are not backdated. The recall cache's
  `mutation_epoch` key component now keeps one comparable scale across
  restarts instead of restarting at 0 with each process.
- `lambo_inspect` no longer refuses the focus approximations a model actually
  types, and recall's rendered text now carries the handle it talks about
  (issue #9). On the dogfood rig inspect failed 28.2% of the time (22 of 78
  calls), every one the bare `no concept matching` refusal, because a caller
  working from recall's rendered block had no way to reach the durable node id:
  the id lived only in `structuredContent`, the text a model actually read
  carried none, and at p90 a concept is 548 B a model will not retype. Four
  changes close the gap, and the 2,000-concept fuzzy-scan cap survives all of
  them with its value, its keying on concept count and the O(total-content)
  lowercase pass it refuses all intact:

  - Recall's rendered block is now
    `<content> [Type] (score N.NN, id <short8>, blast radius N)`, the short
    form being the first 8 hex chars of the node id's simple form. The renderer
    and the resolver share one constant (`format::SHORT_ID_CHARS`), and a
    pure-hex focus of 8..=32 chars now resolves through a short-form id leg in
    `resolve_focus` (unique prefix resolves; several refuse with named
    candidates; none falls through to the content legs), so the token a caller
    reads is itself a valid, durable focus by construction. The floor of 8 is
    the deliberate tradeoff: shorter hex prefixes collide with concept ids
    often enough that hex-looking words like "def" would hijack substring
    foci. The portal's structured hit payload gained an additive `node_id`
    field because its byte-for-byte parity check re-renders the context from
    the payload; the MCP `structuredContent` shapes did not move.
  - A focus that matches nothing now offers near-matches the way the Ambiguous
    arm does: the closest concepts by focus-token overlap, recency breaking
    ties, listed with their node ids under an explicit "suggestions (not
    matches)" header, never a silent match. The ranking is bounded by
    construction (it runs only under the scan cap and keeps at most
    `MAX_INSPECT_CANDIDATES` candidates), a focus sharing no token stays a
    bare refusal, and past the cap the suggestions come from the bounded
    subset only, named as such.
  - Past 2,000 concepts the fuzzy leg no longer refuses outright: it scans a
    bounded subset, the 256 most recently created concepts plus the 256
    highest live blast-radius concepts, deterministic and comparison-only to
    build, and every outcome announces the bound (the fuzzy note, the
    ambiguous message and the refusal all name the subset and the cap). A
    match inside the subset resolves; a match outside it fails honestly as
    Oversized rather than pretending nothing exists. Refusing, not trimming,
    remains the rule for the full pass: it is the fallback that is bounded and
    loud, not the search that is silent.
  - Every failed inspect now records its failure mode and its focus in the
    ledger facts before the error returns (`failure` carrying `missing`,
    `ambiguous` or `oversized`, alongside a focus truncated to 200 characters
    with the explicit marker), so the rig's telemetry can classify Missing,
    Ambiguous and Oversized without reproducing the call. The ledger's
    `error_kind` on these lines deliberately stays `unclassified`: the
    classification rides the new fact, and no new error_kind vocabulary was
    minted.

  Acceptance 4 of the issue, the post-change error rate on the rig, is
  post-deploy measurement and has not been taken; nothing here deploys.

- The H3 cross-store parity suite no longer requires zero rank displacement
  from the `postgres-exact` lane. Issue #2's tie-break change invalidated the
  old contract silently: the midpoint probe of "user schema" and "create
  user" ties exactly in the f64 lane (now settled by canonical key, "creat
  user" first) while pgvector's f32 round-trip sees a strict order the other
  way. Both adapters are faithful to their own scores, so a rank swap whose
  score gap sits inside the f32 round-trip bound (`H3_SCORE_SKEW_EPSILON`)
  in both outputs is now accepted, and any swap across a real gap above the
  bound still fails the suite.
- Every score-ordered list now breaks an exact tie on the concept's
  `canonical_key` before falling back to its node id (issue #2). Node ids
  are minted per run (`Uuid::new_v4`), so an id-settled tie was
  deterministic inside a run and arbitrary across them: the same session
  recalled in a different order, and which synonym duplicate surfaced
  first was seeded by process start, not by the data. The chain is
  **score first, then `canonical_key` ascending, then `NodeId`
  ascending**, stated once in `types::tie_break_by_key` and used by
  recall rank and assemble, the inverted index, daemon rescore and the
  stale-session anchor, canonization window and budget demotion, saints,
  inspect focus, the hybrid merge pick, and the store adapters (sqlite
  and the pg family order in SQL, where sqlite's `BINARY` collation
  makes byte order equal to Rust `str` order; `canonical_key` is
  `NOT NULL` in all three schemas, so the SQL `ASC` can never reach the
  comparator's keyless-sorts-last arm).

  Key *presence* is part of the order — a keyless node sorts after every
  keyed one — because the obvious comparator (compare keys when both are
  present, fall through to the id otherwise) is intransitive on mixed
  input: a keyed node could sort both behind and ahead of the same
  keyless one depending on the third element, so sort results depended
  on input order. The id fallback stays because the key is not a total
  separator — synonym duplicates share one, and interactions carry none
  — but it is final only inside each presence class.

  Three fixes rode along where the chain exposed adjacent defects: the
  inverted index's `remove` now drops the tie-break key entry along with
  the postings, so the map's lifecycle mirrors the postings' it was
  pinned to; the hybrid merge pick moved from the async gather phase
  (which holds no graph, and had been picking by raw id) to the commit
  phase, which validates every tier member as a Concept and picks by
  canonical key; and the web event feed orders same-instant events by
  the moved concept's key, which eval makes routine by stamping every
  event in a cycle with the same `now`.

  Deliberately left id-primary, each named at its site: the
  recent-interactions leg (interactions carry no canonical key),
  presentation orderings with no score to tie (drift hits, conflict and
  high-risk output lists), the hotlist (its `seq` is already a total
  order), and rows a `LIMIT` cut before any tie-break could see them —
  `has_boundary_tie` still forces the exact session query, which is
  authoritative, not unconditionally deterministic.

- `lambo_record_action` embeds the concepts it creates. It had no embedder hop
  at all, so every concept it wrote stored `embedding: NULL` while `derive`'s
  concepts were embedded — an inversion in which everything an agent
  **concluded** was findable by semantic recall and everything it **did** was
  reachable by keyword and graph traversal only. Found by dogfooding on
  2026-09-01: 555 of 946 concepts on the live session carried no vector, and
  the split was categorical rather than partial (Observation 68/68, Logic
  109/109, Constraint 130/130 embedded, all of them `derive`'s; Resource
  394/454 and Entity 161/185 NULL, the action string plus every
  `produces`/`modifies`/`depends_on` entry). It accrued daily rather than
  draining, so it was not a backlog any re-embed could clear.

  Embedding happens off-lock through `embed_action_contents`, with
  `hybrid::derive`'s deadline and its transient/permanent error split, because
  the durable-intent replay's consume-or-keep decision turns on that
  difference. An embedder failure **fails the write** rather than degrading it
  (J3-R3-1): degrading silently to `NULL` is what let this go unnoticed. Both
  the write-queue path (every MCP client) and the new async
  `Memory::record_action_embedded_as` (the CLI's `lambo record-action`) stamp
  the contract first and are gated on `MatchStrategy::Hybrid` — under
  `Canonical` there is no vector leg, and embedding there would stamp a
  contract on a session that asked for none. The synchronous
  `Memory::record_action` keeps its old keyword-only behaviour: a sync
  signature has nowhere to put a model call, so it is now the deliberate
  choice rather than the only one.

- `lambo re-embed --missing-only` backfills concepts that have no vector,
  leaving every existing vector and the session contract untouched. Without it
  a session that accumulated `NULL` vectors inside its own current space had no
  repair path: the full migration refuses to run when the live contract is
  already the stored one ("already carries exactly this contract"), which is
  correct for a migration and is exactly the state a repair runs in. The
  backing `Graph::embed_missing` refuses to overwrite an existing vector (that
  is a migration, and migrations move the contract with them) and appends no
  trailing `SetEmbedding`, so a repair never reads as a migration in the
  mutation log.

- A bounded close could abandon its own flush. `close_bounded` registered a
  *fresh* shutdown listener, so the SIGTERM that started the shutdown could be
  the one that listener recorded — and the close then read it as the operator's
  give-up second press and stopped flushing. `EarlyShutdown` now counts signal
  arrivals rather than latching a flag, so the wind-down and the bounded close
  read one shared record and "a second press" means exactly that. The escape
  hatch is deliberately kept: without it a stalled flush is an unkillable
  process.
- `lambo demo` no longer waits on an exact canonization status. It polled for
  equality every 2ms against a ladder advancing one rung per 25ms cycle, so a
  loaded machine could step Candidate → Venerable *between two polls*, after
  which the wait could never match and burned its full 60s deadline on a state
  that had already passed. It now waits for the concept to have reached **at
  least** the rung in question, which is what the demo means and cannot be
  missed.

- `install.sh` on Apple silicon from an x86_64 (Rosetta) shell installs the
  native arm64 build instead of refusing the machine as Intel macOS: it checks
  `sysctl hw.optional.arm64` when `uname -m` says `x86_64`.

### Notes

- B0's extraction already made `crate::store::pg::{PgStore, Dialect}` and
  `crate::store::pg::cockroach::CockroachDialect` public crate API. 0.3.0 is
  the first release that carries them.
