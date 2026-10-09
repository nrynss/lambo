# Issue #21: bearer token for the OpenAI-compatible embedder

Decisions made while implementing #21, for whoever touches `[embedder]` next.

1. **Override name `LAMBO_EMBED_API_KEY_ENV`.** The issue asked for the key to
   join the `LAMBO_*` list beside `LAMBO_LLAMA_EMBED_URL`. It is named for the
   embedder, not llama.cpp, because the point of #21 is that the adapter is
   protocol-shaped. It holds a variable name, never a token, and is in
   `RESOLVE_ENV_VARS`. The variable it names is chosen by the file, so it
   cannot be listed there; a harness that clears the list and writes no
   `api_key_env` sends no token.
2. **Same name rule as `[serve] token_env` (#32 PR 1).** `[A-Z_][A-Z0-9_]*`, at
   most 64 bytes, not token-shaped; a refused value is never quoted. An inline
   `api_key` is parsed only to be refused, with its value discarded during
   deserialization (`InlineApiKey`), so `Debug` and `Serialize` cannot carry it.
   #21 was written before #32 landed, so it first carried its own copy of the
   rule. After merging main the two copies were folded into
   `src/config/secret_env.rs` (a leaf module under `config`, since both keys
   are `lambo.toml` rules and `config` already depends on `embed`); `token_env`
   and `api_key_env` both call `secret_env::check` and `secret_env::shown`, and
   each keeps its own message wording through `SecretEnvRefusal::message`.
3. **Checked twice.** `EmbedderConfig::overlay_env` refuses a bad name or an
   inline key on the file path, before any later message could quote it;
   `build_embedder` checks again for configs built in code, and is where the
   named variable is read (resolve time, the single construction site).
4. **Refused for other kinds.** `api_key_env` with `candle`, `gemini`,
   `bedrock` or `fixture` is a hard error rather than silently ignored: an
   operator who configured a credential expects it to be used.
5. **`openai` stays `BgeM3`.** It is a parse alias only. The kind displays and
   stamps as `bge_m3`, so no existing `EmbeddingContract` changes; a hosted
   session is told apart by its `model` (for example `@cf/baai/bge-m3`).
6. **`check_health` stays llama.cpp-only** and sends no `Authorization` header.
   Do not wire it into `doctor` or startup for this kind.

7. **Echoes of the token are scrubbed from quoted error bodies, within
   stated limits.** The adapter quotes a non-2xx body into its error; a
   gateway that echoes the presented key would leak it there. Review M2
   widened the first exact-match scrub: `bge_m3::scrub` replaces every run of
   at least 8 consecutive bytes of the token (whole, prefix, suffix or inner),
   in its raw, JSON-escaped (with and without `\/`) and percent-encoded
   (upper/lower hex) forms, in O(n*k) per form, and cuts the quoted body at
   8 KiB. Not detected: runs under 8 bytes (a mask such as `sk-ab...yz`),
   case changes, base64 or `\uXXXX`. A 2xx body that fails to parse is not
   quoted (class, line and column only). Logs, `Debug` and transport errors
   print the URL as scheme, host, port and path, never userinfo or a query.
8. **The token goes only to the configured endpoint.** Redirects are never
   followed (`Policy::none`): reqwest keeps `Authorization` on a same-host,
   same-port redirect even across an https-to-http downgrade. Every 3xx is
   `PermanentConfig` in the J3 status table (300/302 moved there from
   `Unclassified`, since with redirects off a 3xx means the URL is wrong), and
   neither its body nor `Location` is quoted. Proxy env vars are ignored for
   plain http to loopback only; https keeps them (CONNECT tunnels TLS end to
   end, and corporate egress may need it), and plain http elsewhere never
   carries a token. A base URL with a query or fragment is refused.
9. **Lambo's own credential variables are refused by name** for both
   `api_key_env` and `token_env` (`secret_env::LAMBO_CREDENTIAL_ENVS`: the
   three store DSN variables and the three Google credential variables). The
   `[[serve.credential]]` overlap check needs the `[serve]` table, so it runs
   on every `LamboFile` path but not in `build_embedder`, whose
   `EmbedderConfig` carries no serve table; its doc says so.

The gap this note used to record (a parse error quoting the source line, so a
token pasted under a misspelled key was echoed) is closed by #32 PR 1's
`LamboFile::from_toml_str` redaction, now merged into this branch; it covers
`[embedder]` like every other table.
