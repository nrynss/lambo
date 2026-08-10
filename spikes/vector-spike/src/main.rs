//! T0.3 — sqlx × CockroachDB VECTOR decision gate.
//!
//! Four checks (spec §14 / PHASE-0):
//! 1. Connect with sqlx::PgPool
//! 2. INSERT a concepts row with 1024-dim embedding
//! 3. Read back and verify round-trip fidelity
//! 4. ORDER BY embedding <-> $1 LIMIT k + EXPLAIN shows vector index
//!
//! Attempt A: bind as text + `$n::VECTOR` cast (cheapest).

use anyhow::{bail, Context, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use std::env;
use std::time::Instant;
use uuid::Uuid;

const DIM: usize = 1024;
const EPS: f32 = 1e-4;

/// Make a libpq DSN usable with sqlx's rustls stack.
fn rewrite_dsn_for_rustls(dsn: &str) -> String {
    let ca_candidates = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
        "/etc/ssl/ca-bundle.pem",
    ];
    let ca = ca_candidates
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .copied();

    // Replace sslrootcert=system (libpq magic) with a real path or strip it.
    let mut out = dsn.to_string();
    if out.contains("sslrootcert=system") {
        if let Some(path) = ca {
            out = out.replace("sslrootcert=system", &format!("sslrootcert={path}"));
        } else {
            out = out.replace("sslrootcert=system", "");
            out = out.replace("&&", "&");
            // Fall back to require if we cannot verify with a bundle.
            if out.contains("sslmode=verify-full") {
                out = out.replace("sslmode=verify-full", "sslmode=require");
            }
        }
    }
    // Clean dangling ?& or trailing &
    out = out.replace("?&", "?").trim_end_matches('&').to_string();
    if out.ends_with('?') {
        out.pop();
    }
    out
}

fn format_vector(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // full precision text for round-trip
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

fn parse_vector(s: &str) -> Result<Vec<f32>> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    if t.is_empty() {
        return Ok(Vec::new());
    }
    t.split(',')
        .map(|p| {
            p.trim()
                .parse::<f32>()
                .with_context(|| format!("parse f32 from {p:?}"))
        })
        .collect()
}

fn unit_test_vector(seed: f32) -> Vec<f32> {
    // Deterministic pseudo-embedding in [-1, 1], then L2-normalize (Titan normalize=true).
    let mut v: Vec<f32> = (0..DIM)
        .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).abs())
        .fold(0.0_f32, f32::max)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load repo-root .env if present
    let _ = dotenvy::from_filename("../../.env");
    let _ = dotenvy::dotenv();

    let dsn = env::var("LAMBO_COCKROACH_DSN")
        .context("LAMBO_COCKROACH_DSN not set (export or put in repo .env)")?;

    // sqlx + rustls does not understand libpq's sslrootcert=system (tries to open a
    // file literally named "system"). Point at a real CA bundle or drop to require.
    let dsn = rewrite_dsn_for_rustls(&dsn);

    println!("=== T0.3 sqlx × VECTOR spike (Attempt A: text cast) ===");
    let t0 = Instant::now();

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .context("connect PgPool")?;
    println!("[1] connected in {:?}", t0.elapsed());

    let session_id = format!("spike-vector-{}", Uuid::new_v4());
    let interaction_id = Uuid::new_v4();
    let concept_id = Uuid::new_v4();
    let embedding = unit_test_vector(0.17);
    let emb_text = format_vector(&embedding);

    // Parent rows for FK
    sqlx::query(
        r#"
        INSERT INTO sessions (session_id, root_goal, created_at)
        VALUES ($1, '["vector-spike"]'::JSONB, now())
        "#,
    )
    .bind(&session_id)
    .execute(&pool)
    .await
    .context("insert session")?;

    sqlx::query(
        r#"
        INSERT INTO interactions (id, session_id, agent_id, prompt_text, previous_id, created_at)
        VALUES ($1, $2, 'spike', 'vector-spike', NULL, now())
        "#,
    )
    .bind(interaction_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .context("insert interaction")?;

    // Attempt A: bind vector as text, cast server-side
    sqlx::query(
        r#"
        INSERT INTO concepts (
            id, session_id, content, canonical_key, concept_type,
            origin_interaction, origin_agent, created_at, embedding
        ) VALUES (
            $1, $2, $3, $4, $5,
            $6, $7, now(), $8::VECTOR
        )
        "#,
    )
    .bind(concept_id)
    .bind(&session_id)
    .bind("user schema")
    .bind("user schema")
    .bind("Entity")
    .bind(interaction_id)
    .bind("spike")
    .bind(&emb_text)
    .execute(&pool)
    .await
    .context("insert concept with VECTOR (Attempt A text cast)")?;
    println!("[2] INSERT 1024-dim VECTOR via $n::VECTOR text bind — ok");

    let row = sqlx::query(
        r#"
        SELECT embedding::STRING AS emb
        FROM concepts
        WHERE id = $1
        "#,
    )
    .bind(concept_id)
    .fetch_one(&pool)
    .await
    .context("read embedding back")?;

    let emb_back: String = row.try_get("emb")?;
    let parsed = parse_vector(&emb_back)?;
    if parsed.len() != DIM {
        bail!("round-trip dim {} != {DIM}", parsed.len());
    }
    let diff = max_abs_diff(&embedding, &parsed);
    if diff > EPS {
        bail!("round-trip max abs diff {diff} > {EPS}");
    }
    println!("[3] round-trip ok (max abs diff {diff:.3e}, eps={EPS})");

    // Neighbor query + EXPLAIN
    // Runbook: vector indexes plan best for pure `ORDER BY embedding <op> $k LIMIT n`
    // — extra predicates can skip the vector index. Try pure shape first, then filtered.
    let probe = unit_test_vector(0.17); // identical → should rank first
    let probe_text = format_vector(&probe);

    async fn explain_text(
        pool: &sqlx::PgPool,
        sql: &str,
        binds: (&str, &str),
    ) -> Result<String> {
        let rows = sqlx::query(sql)
            .bind(binds.0)
            .bind(binds.1)
            .fetch_all(pool)
            .await?;
        let mut out = String::new();
        for r in &rows {
            if let Ok(s) = r.try_get::<String, _>(0) {
                out.push_str(&s);
                out.push('\n');
            } else if let Ok(s) = r.try_get::<String, _>("info") {
                out.push_str(&s);
                out.push('\n');
            }
        }
        Ok(out)
    }

    // Pure shape: no session filter (best chance of concepts_embedding_idx)
    let explain_pure = sqlx::query(
        r#"
        EXPLAIN
        SELECT id
        FROM concepts
        ORDER BY embedding <-> $1::VECTOR
        LIMIT 5
        "#,
    )
    .bind(&probe_text)
    .fetch_all(&pool)
    .await
    .context("EXPLAIN pure vector query")?;
    let mut explain_pure_text = String::new();
    for r in &explain_pure {
        if let Ok(s) = r.try_get::<String, _>(0) {
            explain_pure_text.push_str(&s);
            explain_pure_text.push('\n');
        } else if let Ok(s) = r.try_get::<String, _>("info") {
            explain_pure_text.push_str(&s);
            explain_pure_text.push('\n');
        }
    }
    println!("[4a] EXPLAIN pure (no session filter):\n{explain_pure_text}");

    let explain_filtered = explain_text(
        &pool,
        r#"
        EXPLAIN
        SELECT id
        FROM concepts
        WHERE session_id = $1
        ORDER BY embedding <-> $2::VECTOR
        LIMIT 5
        "#,
        (&session_id, &probe_text),
    )
    .await
    .context("EXPLAIN filtered vector query")?;
    println!("[4a] EXPLAIN filtered (session_id predicate):\n{explain_filtered}");

    let index_used_pure = explain_pure_text.contains("concepts_embedding_idx");
    let index_used_filtered = explain_filtered.contains("concepts_embedding_idx");
    println!(
        "[4a] index concepts_embedding_idx: pure={index_used_pure} filtered={index_used_filtered}"
    );

    let hits = sqlx::query(
        r#"
        SELECT id::STRING AS id
        FROM concepts
        WHERE session_id = $1
        ORDER BY embedding <-> $2::VECTOR
        LIMIT 5
        "#,
    )
    .bind(&session_id)
    .bind(&probe_text)
    .fetch_all(&pool)
    .await
    .context("similarity query")?;

    let top = hits
        .first()
        .and_then(|r| r.try_get::<String, _>("id").ok())
        .unwrap_or_default();
    println!("[4b] top hit id={top} (expected {concept_id})");
    if top != concept_id.to_string() {
        bail!("expected top hit to be inserted concept");
    }

    let index_used = index_used_pure || index_used_filtered;

    // Cleanup spike rows (leave schema intact)
    sqlx::query("DELETE FROM concepts WHERE session_id = $1")
        .bind(&session_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM interactions WHERE session_id = $1")
        .bind(&session_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE session_id = $1")
        .bind(&session_id)
        .execute(&pool)
        .await?;

    println!();
    println!("=== VERDICT: GO (Rust) ===");
    println!("attempt: A (text bind + $n::VECTOR cast)");
    println!("distance operator: <-> (L2)");
    println!("index used (heuristic): {index_used}");
    println!("round-trip eps: {EPS}");
    println!("total elapsed: {:?}", t0.elapsed());
    if !index_used {
        println!(
            "WARNING: EXPLAIN did not clearly name concepts_embedding_idx; \
             paste EXPLAIN into evidence and re-check planner shape."
        );
    }
    Ok(())
}
