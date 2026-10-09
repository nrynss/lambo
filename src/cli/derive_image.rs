//! `lambo derive-image` (#22 PR 4): one image concept, from a local image file
//! or a client-computed vector file, through the synchronous
//! [`crate::memory::Memory::derive_image_as`], as `lambo derive` goes through
//! `Memory::derive`.
//!
//! The CLI may read local files because the operator is local (design 6.3);
//! the MCP tool never takes a path. The same rules as `lambo_derive_image`
//! apply, from the same `surface` functions, and no message quotes the image
//! bytes, the vector or the vector file's text.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::caps::{check_size_cli, require_nonempty, CliError, ConceptKind};
use super::derive::parse_parent_of;
use super::{close_writer, open_writer};
use crate::graph::image::{self, ImageDerive, ImagePayload};
use crate::resolve::ResolvedBackends;
use crate::surface::image::{
    check_caption_fits, check_submitted_vector, sniff_mime, validate, MAX_IMAGE_BYTES,
};
use crate::types::{ConceptType, EmbeddingContract, Node};

/// Largest `--vector-json` file read, in bytes: room for
/// [`crate::surface::image::MAX_VECTOR_VALUES`] components written out in
/// full, and the contract.
pub const MAX_VECTOR_FILE_BYTES: u64 = 1024 * 1024;

/// Parsed `derive-image` flags.
pub struct Args {
    pub session: String,
    pub agent: String,
    pub caption: String,
    pub kind: ConceptKind,
    pub image_id: Option<String>,
    pub image: Option<PathBuf>,
    pub mime: Option<String>,
    pub vector_json: Option<PathBuf>,
    pub parent_of: Vec<String>,
}

/// The `--vector-json` file: the same shape as `lambo_derive_image`'s
/// `vector`, `{"values": [...], "contract": {"kind", "model", "dim"}}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorFile {
    values: Vec<f32>,
    contract: ContractFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractFile {
    kind: String,
    #[serde(default)]
    model: Option<String>,
    dim: usize,
}

/// Read a local file of at most `cap` bytes, checking the size before
/// reading it.
fn read_capped(flag: &str, path: &Path, cap: u64) -> Result<Vec<u8>, CliError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| CliError::Usage(format!("{flag}: cannot read {}: {e}", path.display())))?;
    if meta.len() > cap {
        return Err(CliError::Usage(format!(
            "{flag}: {} is {} bytes, over the {cap}-byte limit",
            path.display(),
            meta.len()
        )));
    }
    std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("{flag}: cannot read {}: {e}", path.display())))
}

/// Derive one image concept into session memory.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    require_nonempty("caption", &args.caption)?;
    check_size_cli("caption", &args.caption)?;
    image::check_caption(&args.caption).map_err(CliError::Usage)?;
    let concept_type = ConceptType::from(args.kind);
    image::check_image_concept_type(concept_type).map_err(CliError::Usage)?;
    if let Some(id) = &args.image_id {
        image::validate_image_id(id).map_err(CliError::Usage)?;
    }
    check_caption_fits(&args.caption, args.image_id.as_deref()).map_err(CliError::Usage)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    for raw in &args.parent_of {
        check_size_cli("parent-of", raw)?;
        let (parent, child) = parse_parent_of(raw)?;
        check_size_cli("parent_of.parent", &parent)?;
        check_size_cli("parent_of.child", &child)?;
        pairs.push((parent, child));
    }

    // Exactly one source (clap enforces it too; a library caller gets the
    // same rule).
    let bytes;
    let payload = match (&args.image, &args.vector_json) {
        (Some(path), None) => {
            bytes = read_capped("--image", path, MAX_IMAGE_BYTES as u64)?;
            let mime = match &args.mime {
                Some(m) => m.clone(),
                None => sniff_mime(&bytes)
                    .ok_or_else(|| {
                        CliError::Usage("--image: the file is not a PNG, JPEG or WebP image".into())
                    })?
                    .as_str()
                    .to_owned(),
            };
            // An explicit --mime that disagrees with the bytes is refused here.
            ImagePayload::Bytes(validate(&bytes, &mime).map_err(CliError::Usage)?)
        }
        (None, Some(path)) => {
            if args.mime.is_some() {
                return Err(CliError::Usage("--mime goes with --image".into()));
            }
            if !backends.config.accept_client_vectors {
                return Err(CliError::Runtime(
                    "this process does not accept client-computed vectors; enable them with \
                     [embedder] accept_client_vectors = true (or LAMBO_ACCEPT_CLIENT_VECTORS=true)"
                        .into(),
                ));
            }
            let raw = read_capped("--vector-json", path, MAX_VECTOR_FILE_BYTES)?;
            // The parse error's position, never its text: serde quotes the
            // offending value.
            let file: VectorFile = serde_json::from_slice(&raw).map_err(|e| {
                CliError::Usage(format!(
                    "--vector-json must be {{\"values\": [...], \"contract\": {{\"kind\", \
                     \"model\", \"dim\"}}}} with no other keys (line {}, column {})",
                    e.line(),
                    e.column()
                ))
            })?;
            let declared = EmbeddingContract {
                kind: file.contract.kind,
                model: file.contract.model,
                dim: file.contract.dim,
            };
            check_submitted_vector(&file.values, &declared, &backends.embedding)
                .map_err(CliError::Usage)?;
            ImagePayload::Vector {
                values: file.values,
                declared,
            }
        }
        _ => {
            return Err(CliError::Usage(
                "pass exactly one of --image or --vector-json".into(),
            ));
        }
    };

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let derive = ImageDerive {
        caption: &args.caption,
        concept_type,
        image_id: args.image_id.as_deref(),
        payload,
        parent_of: &pair_refs,
        event_time: None,
    };
    let out = match mem.derive_image_as(mem.agent(), derive).await {
        Ok(outcome) => {
            // Name the image concept, so a default (digest) id is learned.
            let g = mem.graph().read();
            let content = outcome
                .created
                .iter()
                .chain(&outcome.matched)
                .find_map(|id| match g.node(*id) {
                    Some(Node::Concept(c)) if c.embedding_source.is_some() => {
                        Some(c.content.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            Ok(format!(
                "derived 1 image concept '{content}': {} created ({} embedded), {} matched existing",
                outcome.created.len(),
                outcome.embedded,
                outcome.matched.len()
            ))
        }
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::embed::{png_with_label, EmbedderConfig, EmbedderKind, FixtureEmbedder};
    use crate::store::{MemoryStore, StoreConfig, StoreKind};
    use crate::test_util::{ScratchDir, VectorSearchable};
    use crate::types::SessionId;

    const SESSION: &str = "cli-derive-image";

    fn backends(store: &Arc<MemoryStore>, accept_client_vectors: bool) -> ResolvedBackends {
        ResolvedBackends {
            store: Box::new(VectorSearchable(Arc::clone(store))),
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

    fn args(caption: &str) -> Args {
        Args {
            session: SESSION.into(),
            agent: "agent-a".into(),
            caption: caption.into(),
            kind: ConceptKind::Resource,
            image_id: Some("r17".into()),
            image: None,
            mime: None,
            vector_json: None,
            parent_of: Vec::new(),
        }
    }

    fn write(dir: &ScratchDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    async fn image_concepts(store: &Arc<MemoryStore>) -> Vec<crate::types::Concept> {
        store
            .load_session(&SessionId::new(SESSION))
            .await
            .map(|s| {
                s.concepts
                    .into_iter()
                    .filter(|c| c.embedding_source.is_some())
                    .collect()
            })
            .unwrap_or_default()
    }

    use crate::store::GraphStore as _;

    #[tokio::test]
    async fn an_image_file_derives_one_image_concept_and_rederives_onto_it() {
        let dir = ScratchDir::new("lambo-cli-derive-image");
        let store = Arc::new(MemoryStore::new());
        let png = write(&dir, "saree.png", &png_with_label("red silk saree"));
        let first = run(
            backends(&store, false),
            Args {
                image: Some(png.clone()),
                ..args("render 17")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            first,
            "derived 1 image concept 'render 17 [image:r17]': 1 created (1 embedded), 0 matched \
             existing"
        );
        let again = run(
            backends(&store, false),
            Args {
                image: Some(png),
                mime: Some("image/png".into()),
                ..args("render 17")
            },
        )
        .await
        .unwrap();
        assert!(
            again.contains("0 created") && again.contains("1 matched"),
            "{again}"
        );
        let [c] = image_concepts(&store).await.try_into().unwrap();
        assert_eq!(
            c.embedding,
            Some(FixtureEmbedder::new().embed_sync("red silk saree"))
        );
    }

    #[tokio::test]
    async fn bad_image_files_and_flags_are_usage_errors_that_write_nothing() {
        let dir = ScratchDir::new("lambo-cli-derive-image-bad");
        let store = Arc::new(MemoryStore::new());
        let png = write(&dir, "x.png", &png_with_label("x"));
        let text = write(&dir, "x.txt", b"SECRET-FILE-TEXT, not an image");
        let big = write(&dir, "big.png", &vec![0u8; MAX_IMAGE_BYTES + 1]);
        let cases: Vec<(Args, &str)> = vec![
            (
                Args {
                    image: Some(png.clone()),
                    mime: Some("image/jpeg".into()),
                    ..args("x")
                },
                "declared image/jpeg but the bytes are image/png",
            ),
            (
                Args {
                    image: Some(text),
                    ..args("x")
                },
                "not a PNG, JPEG or WebP",
            ),
            (
                Args {
                    image: Some(big),
                    ..args("x")
                },
                "over the 2097152-byte limit",
            ),
            (args("x"), "exactly one of --image or --vector-json"),
            (
                Args {
                    image: Some(png.clone()),
                    vector_json: Some(png.clone()),
                    ..args("x")
                },
                "exactly one of --image or --vector-json",
            ),
            (
                Args {
                    image: Some(png.clone()),
                    kind: ConceptKind::Observation,
                    ..args("x")
                },
                "cannot be an observation",
            ),
            (
                Args {
                    image: Some(png.clone()),
                    image_id: Some("Red-17".into()),
                    ..args("x")
                },
                "image_id may contain only",
            ),
            (
                Args {
                    image: Some(png.clone()),
                    ..args("red [image:zzz]")
                },
                "caption may not contain",
            ),
            // Review M2: under the uniform cap, over the caption's real one.
            (
                Args {
                    image: Some(png),
                    ..args(&"c".repeat(16_380))
                },
                "with a 3-byte image_id it may be at most 16372 bytes",
            ),
        ];
        for (a, want) in cases {
            let err = run(backends(&store, false), a).await.unwrap_err();
            assert!(matches!(err, CliError::Usage(_)), "{want}: {err}");
            assert!(err.to_string().contains(want), "{want}: {err}");
            assert!(!err.to_string().contains("SECRET-FILE-TEXT"), "{err}");
        }
        assert!(image_concepts(&store).await.is_empty());
    }

    /// AC4 over the CLI, and the operator opt-in.
    #[tokio::test]
    async fn a_vector_file_needs_the_opt_in_and_the_exact_contract() {
        let dir = ScratchDir::new("lambo-cli-derive-image-vector");
        let store = Arc::new(MemoryStore::new());
        let v = FixtureEmbedder::new().embed_sync("red silk saree");
        let file = |contract: serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({"values": v, "contract": contract})).unwrap()
        };
        let good = write(
            &dir,
            "good.json",
            &file(serde_json::json!({"kind": "fixture", "dim": 1024})),
        );
        let with = |path: &PathBuf| Args {
            vector_json: Some(path.clone()),
            ..args("red silk saree")
        };

        let err = run(backends(&store, false), with(&good)).await.unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)), "{err}");
        assert!(
            err.to_string().contains("accept_client_vectors = true"),
            "{err}"
        );

        let other = write(
            &dir,
            "other.json",
            &file(serde_json::json!({"kind": "fixture", "model": "SECRET-LABEL", "dim": 1024})),
        );
        let err = run(backends(&store, true), with(&other)).await.unwrap_err();
        assert!(err.to_string().contains("(model differs)"), "{err}");
        assert!(!err.to_string().contains("SECRET-LABEL"), "{err}");
        assert!(!err.to_string().contains(&format!("{}", v[0])), "{err}");

        let junk = write(
            &dir,
            "junk.json",
            br#"{"values": "SECRET-VALUE", "contract": {}}"#,
        );
        let err = run(backends(&store, true), with(&junk)).await.unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert!(!err.to_string().contains("SECRET-VALUE"), "{err}");

        let err = run(
            backends(&store, true),
            Args {
                mime: Some("image/png".into()),
                ..with(&good)
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("--mime goes with --image"),
            "{err}"
        );
        assert!(image_concepts(&store).await.is_empty());

        let out = run(backends(&store, true), with(&good)).await.unwrap();
        assert!(
            out.contains("'red silk saree [image:r17]': 1 created"),
            "{out}"
        );
    }

    /// The image-id collision reaches the operator with its fix.
    #[tokio::test]
    async fn an_image_id_held_by_text_is_refused_with_the_fix() {
        let dir = ScratchDir::new("lambo-cli-derive-image-taken");
        let store = Arc::new(MemoryStore::new());
        crate::cli::record_action::run(
            backends(&store, false),
            crate::cli::record_action::Args {
                session: SESSION.into(),
                agent: "agent-a".into(),
                action: "dismissed the outfit".into(),
                produces: vec![],
                modifies: vec![],
                depends_on: vec!["render 17 [image:r17]".into()],
            },
        )
        .await
        .unwrap();
        let png = write(&dir, "saree.png", &png_with_label("red silk saree"));
        let err = run(
            backends(&store, false),
            Args {
                image: Some(png),
                ..args("render 17")
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)), "{err}");
        assert!(err.to_string().contains("another image id"), "{err}");
        assert!(image_concepts(&store).await.is_empty());
    }
}
