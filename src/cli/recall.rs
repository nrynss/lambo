//! `lambo recall` — lease-free reader process (spec §2.2).
//!
//! Loads the session, builds a [`Daemon`] **without spawning** (spawn would run
//! GC = writer), embeds the query only when the store claims `VECTOR_SEARCH`,
//! and prints the T5.3 context block.
//!
//! H3: the public [`run`] stays a thin wrapper over `run_detailed`, the
//! single-execution seam that produces BOTH the operator-visible string and
//! the structured presentation model the HTTP `/api/recall` payload is
//! serialized from. The CLI string and the HTTP `context` are the same
//! execution's output by construction.

use super::caps::{
    check_in_range_cli, check_size_cli, clamp_cfg_default, require_nonempty, CliError,
    MAX_MAX_TOKENS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH,
};
use super::load_reader_graph_with_contract;
use crate::config::Config;
use crate::daemon::{Daemon, RecallPipeline};
use crate::recall::cache::RecallCache;
use crate::recall::candidates;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::recall::query_vector::{self, QueryBy};
use crate::resolve::ResolvedBackends;
use crate::store::vector_source::VectorCandidates;
use crate::surface::image::{check_submitted_vector_as, sniff_mime, validate, MAX_IMAGE_BYTES};
use crate::types::RecallQuery;

use super::derive_image::{read_capped, VectorFile, MAX_VECTOR_FILE_BYTES};
use std::path::PathBuf;

/// The result of one detailed recall: the full `lambo recall` string plus the
/// H3 presentation model, all from the same execution.
pub(crate) struct CliRecall {
    /// The complete `lambo recall` output — the ⚑ header (if any) above the
    /// rendered context blocks. This is byte-identical to what [`run`]
    /// returns and what `/api/recall` puts in `context`.
    pub(crate) context: String,
    /// The presentation hits, serialized on the wire as `hits`.
    pub(crate) hits: Vec<DetailedHit>,
    /// Response-global annotations in producer order: `vector_degraded`
    /// (query embedding failure, CLI side) first, then the daemon's own
    /// (`traversal` for a dispatched structural query).
    pub(crate) response_annotations: Vec<Annotation>,
}

/// Recall relevant memory for a query.
pub async fn run(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<String, CliError> {
    Ok(
        run_detailed(backends, session, query, top_k, max_tokens, traversal_depth)
            .await?
            .context,
    )
}

/// What `lambo recall` searches its vector leg by instead of the query
/// text's embedding (#22 PR 6): a local image file or a client vector file.
/// At most one of the two; with either, the text is optional.
#[derive(Debug, Default)]
pub struct RecallBy {
    /// A PNG, JPEG or WebP file for the configured embedder to embed.
    pub image: Option<PathBuf>,
    /// The image's MIME type; default, the type its magic bytes name.
    pub mime: Option<String>,
    /// A `{"values": [...], "contract": {...}}` file, as `lambo
    /// derive-image --vector-json` takes.
    pub query_vector_json: Option<PathBuf>,
}

/// [`run`], recalling by an image or a client vector file (#22 PR 6).
///
/// The files are read under the same caps as `lambo derive-image`'s
/// (through `take(cap + 1)`, so a pipe or a growing file cannot exceed
/// them), checked by the same `surface` rules, and never echoed. A vector
/// file needs `[embedder] accept_client_vectors = true` and the live
/// contract exactly. Nothing is cached and the vector leg is required: a
/// store without vector search, or an image the embedder cannot embed, is
/// an error rather than a keyword-only answer.
pub async fn run_by(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    by: &RecallBy,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<String, CliError> {
    if by.mime.is_some() && by.image.is_none() {
        return Err(CliError::Usage("--mime goes with --image".into()));
    }
    let bytes;
    let query_by = match (&by.image, &by.query_vector_json) {
        (None, None) => None,
        (Some(path), None) => {
            bytes = read_capped("--image", path, MAX_IMAGE_BYTES as u64)?;
            let mime = match &by.mime {
                Some(m) => m.clone(),
                None => sniff_mime(&bytes)
                    .ok_or_else(|| {
                        CliError::Usage("--image: the file is not a PNG, JPEG or WebP image".into())
                    })?
                    .as_str()
                    .to_owned(),
            };
            // An explicit --mime that disagrees with the bytes is refused.
            Some(QueryBy::Image(
                validate(&bytes, &mime).map_err(CliError::Usage)?,
            ))
        }
        (None, Some(path)) => {
            if !backends.config.accept_client_vectors {
                return Err(CliError::Runtime(
                    "this process does not accept client-computed vectors; enable them with \
                     [embedder] accept_client_vectors = true (or LAMBO_ACCEPT_CLIENT_VECTORS=true)"
                        .into(),
                ));
            }
            let raw = read_capped("--query-vector-json", path, MAX_VECTOR_FILE_BYTES)?;
            let (values, declared) = VectorFile::parse("--query-vector-json", &raw)?;
            check_submitted_vector_as("query_vector", &values, &declared, &backends.embedding)
                .map_err(CliError::Usage)?;
            Some(QueryBy::Vector { values, declared })
        }
        (Some(_), Some(_)) => {
            return Err(CliError::Usage(
                "pass at most one of --image or --query-vector-json".into(),
            ));
        }
    };
    Ok(run_inner(
        backends,
        session,
        query,
        query_by,
        top_k,
        max_tokens,
        traversal_depth,
    )
    .await?
    .context)
}

/// One recall execution producing the CLI string AND the H3 presentation
/// model. The HTTP endpoint calls this instead of [`run`], so the page's
/// `context` can never drift from `lambo recall` — both project from the
/// same execution's data.
pub(crate) async fn run_detailed(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<CliRecall, CliError> {
    run_inner(
        backends,
        session,
        query,
        None,
        top_k,
        max_tokens,
        traversal_depth,
    )
    .await
}

/// [`run_detailed`], optionally searching the vector leg by `by` (#22 PR 6).
async fn run_inner(
    backends: &ResolvedBackends,
    session: &str,
    query: &str,
    by: Option<QueryBy<'_>>,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
) -> Result<CliRecall, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    // Beside an image or a vector the text is optional; blank is none.
    let query = if by.is_some() && query.trim().is_empty() {
        ""
    } else {
        require_nonempty("query", query)?;
        query
    };
    check_size_cli("query", query)?;
    // #22 PR 6: the vector leg is the point of a recall by image or vector,
    // so a store that cannot search vectors is an error, before any I/O.
    if by.is_some() && !VectorCandidates::from_store(backends.store.as_ref()).available() {
        return Err(CliError::Runtime(
            "a recall by image or by vector needs a store with vector search \
             (VECTOR_SEARCH), and this store does not search vectors"
                .into(),
        ));
    }

    let cfg = Config::default();
    let top_k = match top_k {
        Some(v) => v,
        None => clamp_cfg_default("default_top_k", cfg.default_top_k, 1, MAX_TOP_K),
    };
    let max_tokens = match max_tokens {
        Some(v) => v,
        None => clamp_cfg_default(
            "default_max_tokens",
            cfg.default_max_tokens,
            1,
            MAX_MAX_TOKENS,
        ),
    };
    let traversal_depth = match traversal_depth {
        Some(v) => v,
        None => clamp_cfg_default(
            "default_traversal_depth",
            cfg.default_traversal_depth,
            0,
            MAX_TRAVERSAL_DEPTH,
        ),
    };
    check_in_range_cli("top-k", top_k, 1, MAX_TOP_K)?;
    check_in_range_cli("traversal-depth", traversal_depth, 0, MAX_TRAVERSAL_DEPTH)?;
    check_in_range_cli("max-tokens", max_tokens, 1, MAX_MAX_TOKENS)?;

    let loaded = load_reader_graph_with_contract(
        backends.store.as_ref(),
        session,
        Some(&backends.embedding),
    )
    .await?;
    // Do NOT spawn: spawn would run GC, which is a writer. Config::default()
    // for knobs — same as `lambo serve` today (T82-12 is not T8.3's to fix).
    let daemon = Daemon::from_config(loaded.graph, &cfg).with_index(loaded.index);

    // H3: the embed-failure line is a typed, response-global annotation
    // (`vector_degraded`) captured at its producer — never text-parsed later.
    let mut extra_annotations: Vec<Annotation> = Vec::new();
    // The same embed step `Memory::recall_detailed` uses (#27). The source is
    // the store: this is a reader, and the graph-backed source (#8) is chosen
    // only by a session holder (`VectorCandidates::for_holder`), whose graph is
    // the freshest copy. Over the same flushed state the two rank the same,
    // but the store read below is a later transaction than the snapshot load
    // above: if a writer (`lambo serve`) flushes in between, the store's
    // vector hits can include concepts this snapshot lacks, and assembly skips
    // them. Ranking against the snapshot's own graph instead would keep the
    // vector leg consistent with assembly and drop the second vector parse
    // (a follow-up in dev-diary/notes/feature-8-vector-source.md).
    let vectors = VectorCandidates::from_store(backends.store.as_ref());
    let mut cache = RecallCache::<RecallPipeline>::new();
    let rq = RecallQuery {
        query: query.to_string(),
        top_k,
        max_tokens,
        traversal_depth,
    };
    let mut detail = match by {
        None => {
            let embedding =
                match candidates::embed_query(vectors, backends.embedder.as_ref(), query).await {
                    Ok(vector) => vector,
                    Err(text) => {
                        extra_annotations
                            .push(Annotation::new(AnnotationKind::VectorDegraded, text));
                        None
                    }
                };
            daemon
                .recall_with(
                    &loaded.session,
                    rq,
                    vectors,
                    embedding
                        .as_deref()
                        .map(|vector| (vector, &backends.embedding)),
                    cfg.recall_weights,
                    &mut cache,
                )
                .await
        }
        // #22 PR 6: the vector leg is the point, so no vector search is an
        // error, not a degradation; and nothing is cached (this process's
        // cache is discarded anyway).
        Some(by) => {
            let vector =
                query_vector::resolve(by, backends.embedder.as_ref(), &backends.embedding).await?;
            daemon
                .recall_by_vector_with(
                    &loaded.session,
                    rq,
                    vectors,
                    (&vector, &backends.embedding),
                    cfg.recall_weights,
                    &mut cache,
                )
                .await
        }
    };
    // Response-global annotations preserve producer order: the CLI-side
    // `vector_degraded` (embedded before recall) precedes the daemon's
    // (`traversal`, produced during recall).
    extra_annotations.append(&mut detail.response_annotations);
    detail.response_annotations = extra_annotations;

    let context = render_cli_text(&detail);
    Ok(CliRecall {
        context,
        hits: detail.detailed,
        response_annotations: detail.response_annotations,
    })
}

/// Render the operator-visible `lambo recall` string from a detailed recall —
/// the single renderer the CLI and the HTTP payload share. The context blocks
/// are the included hits' own rendered blocks (the pipeline's exact block
/// format, see [`crate::recall::format::render_detailed_block`]); the header
/// carries every warning whose owning block is outside the token budget,
/// preserving producer order: `vector_degraded` first, then each hit's
/// annotations in rank order, then the remaining response-global annotations
/// (`traversal`), then any pipeline `warnings` no annotation already carries
/// (E2E-5 — a warnings-only producer must never render empty output). A
/// warning line whose block IS in the context is not duplicated in the
/// header, and a warning text that a typed annotation already rendered is
/// not duplicated either.
///
/// The parity this enforces is the H3 losslessness property: every annotation
/// and warning text appears in the output exactly once — inside its included
/// block, or as a header line when the block was excluded.
pub(crate) fn render_cli_text(detail: &DetailedRecall) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for h in &detail.detailed {
        if h.included_in_context {
            blocks.push(crate::recall::format::render_detailed_block(h));
        }
    }
    let block_context = blocks.join("\n\n");

    let mut header = String::new();
    let mut push_header = |w: &str| {
        if w.contains('⚑') {
            header.push_str(w);
        } else {
            header.push('⚑');
            header.push(' ');
            header.push_str(w);
        }
        header.push('\n');
    };
    for a in &detail.response_annotations {
        if a.kind == AnnotationKind::VectorDegraded {
            push_header(&a.text);
        }
    }
    for h in &detail.detailed {
        if h.included_in_context {
            continue;
        }
        for a in &h.annotations {
            push_header(&a.text);
        }
    }
    for a in &detail.response_annotations {
        if a.kind != AnnotationKind::VectorDegraded {
            push_header(&a.text);
        }
    }
    // E2E-5: pipeline warnings that no annotation already rendered. Every
    // warning a hit carries is also a typed annotation (assemble attaches
    // both), so the annotation-rendered set is the exact skip set — a plain
    // warning such as the warn_only refusal paths or the missing-index note
    // is neither, and would otherwise vanish from CLI and HTTP output.
    let annotated: std::collections::HashSet<&str> = detail
        .response_annotations
        .iter()
        .map(|a| a.text.as_str())
        .chain(
            detail
                .detailed
                .iter()
                .flat_map(|h| h.annotations.iter().map(|a| a.text.as_str())),
        )
        .collect();
    for w in &detail.warnings {
        if !annotated.contains(w.as_str()) {
            push_header(w);
        }
    }

    if header.is_empty() {
        block_context
    } else if block_context.is_empty() {
        header
    } else {
        format!("{header}{block_context}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2E-5: a warnings-only detailed result (the daemon's early refusal
    /// paths — limit validation, session mismatch — and the missing-index
    /// note) carries its message in `warnings` with empty `detailed` and no
    /// annotations. It must render non-empty, or the warning would silently
    /// vanish from both the CLI string and the HTTP `context`.
    #[test]
    fn a_warnings_only_detailed_result_renders_non_empty() {
        let warning = "recall: top-k validation failed; refusing to search";
        let detail = DetailedRecall::warn_only(warning.to_string());
        let text = render_cli_text(&detail);
        assert!(!text.is_empty(), "a warnings-only result must render");
        assert!(
            text.contains(warning),
            "the warning text must survive: {text:?}"
        );
        assert!(
            text.starts_with('⚑'),
            "a bare warning is ⚑-prefixed: {text:?}"
        );
    }

    /// E2E-5: a warning whose text an annotation already rendered must not be
    /// duplicated into the header (the H3 losslessness property). The
    /// embed-failure path and every per-hit warning are annotated; the
    /// warnings list carries the same text, and the skip set is the exact
    /// annotation-rendered set.
    #[test]
    fn a_warning_covered_by_an_annotation_is_not_duplicated() {
        let warning = "recall: query embedding failed (boom); vector leg skipped";
        let mut detail = DetailedRecall::warn_only(warning.to_string());
        detail
            .response_annotations
            .push(Annotation::new(AnnotationKind::VectorDegraded, warning));
        let text = render_cli_text(&detail);
        assert_eq!(
            text.matches(warning).count(),
            1,
            "annotated warning renders exactly once: {text:?}"
        );
    }

    /// E2E-5: a warning with no annotation renders even beside an included
    /// block — the header is additive, never swallowed by the context.
    #[test]
    fn an_unannotated_warning_renders_beside_context_blocks() {
        let mut detail = DetailedRecall::warn_only(
            "recall: no inverted index installed (Daemon::with_index) - keyword leg unavailable"
                .to_string(),
        );
        detail.warnings.push("recall: second note".to_string());
        let text = render_cli_text(&detail);
        assert_eq!(
            text.lines().count(),
            2,
            "one header line per warning: {text:?}"
        );
        assert!(text.contains("recall: second note"), "{text:?}");
    }
}

/// #22 PR 6: `lambo recall --image | --query-vector-json`'s refusals, on
/// the memory store. The end-to-end ranking runs on SQLite
/// (`store::sqlite::tests::image_e2e`), whose checked read really ranks.
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod by_tests {
    use std::sync::Arc;

    use super::*;
    use crate::embed::{png_with_label, EmbedderConfig, EmbedderKind, FixtureEmbedder};
    use crate::store::{GraphStore, MemoryStore, StoreConfig, StoreKind};
    use crate::test_util::{ScratchDir, VectorSearchable};
    use crate::types::EmbeddingContract;

    const SECRET_MODEL: &str = "client-declared-model-label";

    fn backends(store: Box<dyn GraphStore>, accept_client_vectors: bool) -> ResolvedBackends {
        ResolvedBackends {
            store,
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
                vector_dim: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                accept_client_vectors,
                ..Default::default()
            },
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
            allow_embedding_mismatch: false,
            config: crate::Config {
                accept_client_vectors,
                ..crate::Config::default()
            },
        }
    }

    fn searchable(accept: bool) -> ResolvedBackends {
        backends(
            Box::new(VectorSearchable(Arc::new(MemoryStore::new()))),
            accept,
        )
    }

    fn write(dir: &ScratchDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    async fn refused(b: &ResolvedBackends, query: &str, by: RecallBy) -> CliError {
        run_by(b, "cli-recall-by", query, &by, Some(5), None, None)
            .await
            .expect_err("refused")
    }

    fn vector_file(dir: &ScratchDir, name: &str, contract: serde_json::Value) -> PathBuf {
        let v = FixtureEmbedder::new().embed_sync("red silk saree");
        let body = serde_json::json!({"values": v, "contract": contract});
        write(dir, name, body.to_string().as_bytes())
    }

    #[tokio::test]
    async fn the_flags_files_and_opt_in_are_checked_before_any_recall() {
        let dir = ScratchDir::new("lambo-cli-recall-by");
        let png = write(&dir, "q.png", &png_with_label("red silk saree"));
        let big = write(&dir, "big.png", &vec![0u8; MAX_IMAGE_BYTES + 1]);
        let text = write(&dir, "q.txt", b"SECRET-FILE-TEXT, not an image");
        let good = vector_file(
            &dir,
            "v.json",
            serde_json::json!({"kind": "fixture", "dim": 1024}),
        );
        let other = vector_file(
            &dir,
            "other.json",
            serde_json::json!({"kind": "fixture", "model": SECRET_MODEL, "dim": 1024}),
        );
        let b = searchable(true);

        let usage = |e: CliError| match e {
            CliError::Usage(m) => m,
            other => panic!("usage error: {other:?}"),
        };
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    image: Some(png.clone()),
                    query_vector_json: Some(good.clone()),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(
            m.contains("at most one of --image or --query-vector-json"),
            "{m}"
        );
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    mime: Some("image/png".into()),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(m.contains("--mime goes with --image"), "{m}");
        // Neither file: the text is required, as for plain `lambo recall`.
        let m = usage(refused(&b, "  ", RecallBy::default()).await);
        assert!(m.contains("query"), "{m}");
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    image: Some(big),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(m.contains("over the 2097152-byte limit"), "{m}");
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    image: Some(text),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(m.contains("not a PNG, JPEG or WebP"), "{m}");
        assert!(!m.contains("SECRET-FILE-TEXT"), "{m}");
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    image: Some(png),
                    mime: Some("image/jpeg".into()),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(
            m.contains("declared image/jpeg but the bytes are image/png"),
            "{m}"
        );
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    query_vector_json: Some(other),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(m.starts_with("query_vector.contract"), "{m}");
        assert!(m.contains("(model differs)"), "{m}");
        assert!(!m.contains(SECRET_MODEL), "{m}");
        let malformed = write(&dir, "bad.json", b"{\"values\": [\"SECRET-VALUE\"]}");
        let m = usage(
            refused(
                &b,
                "",
                RecallBy {
                    query_vector_json: Some(malformed),
                    ..Default::default()
                },
            )
            .await,
        );
        assert!(m.contains("line 1"), "{m}");
        assert!(!m.contains("SECRET-VALUE"), "{m}");

        // Client vectors off: refused naming the key.
        let off = searchable(false);
        let e = refused(
            &off,
            "",
            RecallBy {
                query_vector_json: Some(good.clone()),
                ..Default::default()
            },
        )
        .await;
        let CliError::Runtime(m) = e else {
            panic!("runtime error: {e:?}")
        };
        assert!(m.contains("[embedder] accept_client_vectors = true"), "{m}");

        // A store without vector search: refused naming the capability.
        let plain = backends(Box::new(MemoryStore::new()), true);
        let e = refused(
            &plain,
            "",
            RecallBy {
                query_vector_json: Some(good),
                ..Default::default()
            },
        )
        .await;
        let CliError::Runtime(m) = e else {
            panic!("runtime error: {e:?}")
        };
        assert!(m.contains("VECTOR_SEARCH"), "{m}");
    }
}
