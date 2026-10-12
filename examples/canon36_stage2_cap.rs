//! #36 PR 2: measure what the Stage 2 coverage cap changes on a SQLite store.
//!
//! For every concept in one session, compares Stage 2 under the raw
//! whole-extent denominator (`GraphStore::interaction_span`) with Stage 2
//! under a capped one (`interaction_span_with_window`), through the real
//! store queries, and prints the counts plus one line per concept whose
//! verdict the cap flips (node id, type, status, origins, both coverages and
//! blast radius; never concept text).
//!
//! The database is opened **read-only**. Still, run it on a copy of a store,
//! never on a live writer's file.
//!
//! ```text
//! cargo run --features store-sqlite --example canon36_stage2_cap -- \
//!     <store.db> <session-id> [--min-age-secs 60] [--window-days 14] [--at <RFC3339>]
//! ```
//!
//! `--at` pins the evaluation clock (default: now), so two runs over the same
//! copy agree. `--window-days` accepts a fraction (at least one day, the
//! cap's floor).

#[cfg(not(feature = "store-sqlite"))]
fn main() {
    eprintln!("canon36_stage2_cap needs --features store-sqlite");
    std::process::exit(2);
}

#[cfg(feature = "store-sqlite")]
#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("canon36_stage2_cap: {message}");
        std::process::exit(2);
    }
}

#[cfg(feature = "store-sqlite")]
async fn run() -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use std::time::Duration;

    use lambo::store::{GraphStore, SqliteStore};
    use lambo::types::SessionId;
    use sqlx::sqlite::SqliteConnectOptions;

    let usage = "usage: canon36_stage2_cap <store.db> <session-id> \
                 [--min-age-secs N] [--window-days D] [--at RFC3339]";
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or(usage)?;
    let session = SessionId::from(args.next().ok_or(usage)?.as_str());
    let mut min_age = Duration::from_secs(60);
    let mut window = lambo::store::DEFAULT_COVERAGE_WINDOW;
    let mut now = chrono::Utc::now();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(usage)?;
        match flag.as_str() {
            "--min-age-secs" => {
                min_age = Duration::from_secs(value.parse().map_err(|e| format!("{flag}: {e}"))?)
            }
            "--window-days" => {
                let days: f64 = value.parse().map_err(|e| format!("{flag}: {e}"))?;
                window = Duration::from_millis((days * 86_400_000.0).round() as u64);
            }
            "--at" => {
                now = chrono::DateTime::parse_from_rfc3339(&value)
                    .map_err(|e| format!("{flag}: {e}"))?
                    .with_timezone(&chrono::Utc)
            }
            other => return Err(format!("unknown flag {other}; {usage}")),
        }
    }
    lambo::store::validate_coverage_window(window).map_err(|e| e.to_string())?;

    let options = SqliteConnectOptions::from_str(&path)
        .map_err(|e| format!("{path}: {e}"))?
        .read_only(true);
    let store = SqliteStore::new(options);
    let snapshot = store
        .load_session(&session)
        .await
        .map_err(|e| e.to_string())?;
    if snapshot.concepts.is_empty() {
        return Err(format!("session {session:?} has no concepts in {path}"));
    }

    let passes = |distinct: u64, coverage: f64| distinct >= 3 && coverage >= 0.3;
    let (mut three_origins, mut raw_pass, mut capped_pass) = (0usize, 0usize, 0usize);
    let mut flipped_by: BTreeMap<String, usize> = BTreeMap::new();
    let mut flips = Vec::new();
    for concept in &snapshot.concepts {
        let raw = store
            .interaction_span(&session, concept.id, min_age, now)
            .await
            .map_err(|e| e.to_string())?;
        let capped = store
            .interaction_span_with_window(&session, concept.id, min_age, now, window)
            .await
            .map_err(|e| e.to_string())?;
        if raw.distinct != capped.distinct || capped.coverage + 1e-12 < raw.coverage {
            return Err(format!(
                "{}: capped evidence disagrees with raw ({raw:?} vs {capped:?})",
                concept.id
            ));
        }
        three_origins += usize::from(raw.distinct >= 3);
        let (was, is) = (
            passes(raw.distinct, raw.coverage),
            passes(capped.distinct, capped.coverage),
        );
        raw_pass += usize::from(was);
        capped_pass += usize::from(is);
        if was != is {
            let blast = store
                .blast_radius(&session, concept.id, min_age, now)
                .await
                .map_err(|e| e.to_string())?;
            *flipped_by
                .entry(format!(
                    "{:?}/{:?}",
                    concept.concept_type, concept.canonization_status
                ))
                .or_default() += 1;
            flips.push(format!(
                "{} {:?} {:?} origins={} raw={:.4} capped={:.4} blast={blast}",
                concept.id,
                concept.concept_type,
                concept.canonization_status,
                raw.distinct,
                raw.coverage,
                capped.coverage,
            ));
        }
    }

    println!(
        "session={session} at={} min_age={min_age:?} window={window:?}",
        now.to_rfc3339()
    );
    println!(
        "concepts={} with>=3_origins={three_origins} stage2_raw={raw_pass} \
         stage2_capped={capped_pass}",
        snapshot.concepts.len()
    );
    println!("flipped by type/status: {flipped_by:?}");
    for line in flips {
        println!("flipped: {line}");
    }
    Ok(())
}
