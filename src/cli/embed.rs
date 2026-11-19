//! Print vectors from the configured embedder without opening a session.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use serde::Serialize;

use super::caps::{check_size_cli, require_nonempty, CliError, MAX_CONTENT_BYTES};
use crate::graph::hybrid::context_text;
use crate::resolve::ResolvedEmbedder;
use crate::types::{check_supplied_values, EmbeddingContract};

/// Text to embed in the role used by recall or derive.
pub enum Input<'a> {
    Query(&'a str),
    Document {
        text: &'a str,
        origin: Option<&'a str>,
    },
    QueriesFile(&'a Path),
}

enum Role {
    Query,
    Document,
}

#[derive(Serialize)]
struct VectorRecord<'a> {
    values: &'a [f32],
    contract: &'a EmbeddingContract,
}

/// Write a vector JSON object per input, in input order.
pub async fn run(
    resolved: &ResolvedEmbedder,
    input: Input<'_>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    match input {
        Input::Query(text) => emit(resolved, Role::Query, "--query", text, out).await?,
        Input::Document { text, origin } => {
            if let Some(origin) = origin {
                check_size_cli("--document", text)?;
                require_nonempty("--document", text)?;
                check_size_cli("--origin", origin)?;
                let framed = context_text(text, Some(origin));
                emit(resolved, Role::Document, "framed document", &framed, out).await?;
            } else {
                emit(resolved, Role::Document, "--document", text, out).await?;
            }
        }
        Input::QueriesFile(path) => {
            let file = File::open(path).map_err(|e| {
                CliError::Usage(format!(
                    "--queries-file: cannot open {}: {e}",
                    path.display()
                ))
            })?;
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            for number in 1_usize.. {
                line.clear();
                let field = format!("--queries-file line {number}");
                // Include room for CRLF and one overflow byte; a long line
                // must not allocate past the shared text limit.
                let bytes = (&mut reader)
                    .take(MAX_CONTENT_BYTES as u64 + 3)
                    .read_line(&mut line)
                    .map_err(|e| CliError::Usage(format!("{field}: cannot read: {e}")))?;
                if bytes == 0 {
                    break;
                }
                let text = line
                    .strip_suffix("\r\n")
                    .or_else(|| line.strip_suffix('\n'))
                    .unwrap_or(&line);
                emit(resolved, Role::Query, &field, text, out).await?;
            }
        }
    }
    out.flush()
        .map_err(|e| CliError::Runtime(format!("cannot flush vector JSON: {e}")))
}

async fn emit(
    resolved: &ResolvedEmbedder,
    role: Role,
    field: &str,
    text: &str,
    out: &mut impl Write,
) -> Result<(), CliError> {
    require_nonempty(field, text)?;
    check_size_cli(field, text)?;
    let values = match role {
        Role::Query => resolved.embedder.embed_query(text).await,
        Role::Document => resolved.embedder.embed(text).await,
    }
    .map_err(|e| CliError::Runtime(format!("{field}: embedding failed: {e}")))?;
    let contract = &resolved.embedding;
    // Refuse malformed backend output before serde could emit a null float.
    check_supplied_values(&values, contract, contract)
        .map_err(|e| CliError::Runtime(format!("{field}: invalid embedder output: {e}")))?;
    serde_json::to_writer(
        &mut *out,
        &VectorRecord {
            values: &values,
            contract,
        },
    )
    .map_err(|e| CliError::Runtime(format!("cannot write vector JSON: {e}")))?;
    out.write_all(b"\n")
        .map_err(|e| CliError::Runtime(format!("cannot write vector JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{EmbedError, Embedder};

    struct Asymmetric;

    #[async_trait::async_trait]
    impl Embedder for Asymmetric {
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed(&self, _: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![0.0, 1.0])
        }
        async fn embed_query(&self, _: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![1.0, 0.0])
        }
    }

    #[tokio::test]
    async fn embed_uses_the_requested_role_on_an_asymmetric_adapter() {
        let resolved = ResolvedEmbedder {
            embedder: Box::new(Asymmetric),
            embedding: EmbeddingContract {
                kind: "test".into(),
                model: Some("asymmetric".into()),
                dim: 2,
            },
        };
        for (input, values) in [
            (Input::Query("policy"), [1.0, 0.0]),
            (
                Input::Document {
                    text: "policy",
                    origin: None,
                },
                [0.0, 1.0],
            ),
        ] {
            let mut bytes = Vec::new();
            run(&resolved, input, &mut bytes).await.unwrap();
            let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(record["values"], serde_json::json!(values));
            assert_eq!(record["contract"]["model"], "asymmetric");
        }
    }
}
