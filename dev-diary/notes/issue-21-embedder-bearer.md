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
   The helpers live in `src/embed/api_key.rs` and duplicate the ones #32 puts
   in `src/config/serve.rs`, because #32 had not landed when #21 was written.
   Fold them into one module once both are on main.
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

Known gap, not fixed here: on main, `LamboFile::from_toml_str` formats the TOML
error with `{e}`, which quotes the offending source line. A token pasted under
a misspelled key (`api_kye = "..."`) would be echoed. #32 PR 1 (commits
"never echo a token pasted into token_env" and "keep values out of kind and
wrong-type errors") replaces that formatting for every table, `[embedder]`
included, so it is not duplicated here.
