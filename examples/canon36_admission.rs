//! #36 PR 3: what evidence-led Swarm admission and evidence-gated Solo do on
//! a SQLite store, measured on a copy.
//!
//! For one session, under each policy, it reports:
//!
//! 1. **First evaluation, unbatched.** For every concept, whether its next
//!    hop's gates hold now (`canon::explain_admission`, the evaluator's own
//!    predicates, against the real SQLite queries): how many would newly
//!    become Candidate, Venerable or Canonical on the first evaluation if the
//!    batch did not bind, split Resource versus non-Resource.
//! 2. **First cycle.** One real `Evaluator::eval_cycle` with the default
//!    parameters (batch 50) on an in-memory copy of the session.
//! 3. **Fixed point.** The same evaluator run until it commits nothing for
//!    `--quiet` cycles (61 s apart, rescoring before each), with the final
//!    status counts by concept type.
//!
//! Every concept that would turn Canonical is listed by node id and type.
//! The program prints **no concept text**, only ids, short type and status
//! labels and counts, and it holds no data of its own. The SQLite database is
//! opened **read-only**; writes go to the in-memory copy. Still, run it on a
//! copy of a store, never on a live writer's file.
//!
//! ```text
//! cargo run --features store-sqlite --example canon36_admission -- \
//!     <store.db> <session-id> [--at <RFC3339>] [--assume-survived] [--quiet 30]
//! ```
//!
//! `--at` pins the evaluation clock (default: now). `--assume-survived`
//! raises every `gc_survived` below 3 to 3 first, to show the state after
//! three GC sweeps (Swarm's Stage 1 needs it; Solo does not read it).

#[cfg(not(all(feature = "store-sqlite", feature = "store-memory")))]
fn main() {
    eprintln!("canon36_admission needs --features store-sqlite and the default store-memory");
    std::process::exit(2);
}

#[cfg(all(feature = "store-sqlite", feature = "store-memory"))]
#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("canon36_admission: {message}");
        std::process::exit(2);
    }
}

#[cfg(all(feature = "store-sqlite", feature = "store-memory"))]
async fn run() -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use lambo::canon::{
        admission_inputs, explain_admission, EvalParams, Evaluator, PromotionPolicy,
    };
    use lambo::store::{GraphStore, MemoryStore, SqliteStore};
    use lambo::types::{
        CanonizationStatus, ConceptType, Mutation, MutationBatch, Node, ScoreTable, SessionId,
    };
    use lambo::{Graph, ScoringWeights};
    use parking_lot::RwLock;
    use sqlx::sqlite::SqliteConnectOptions;

    let usage = "usage: canon36_admission <store.db> <session-id> \
                 [--at RFC3339] [--assume-survived] [--quiet N]";
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or(usage)?;
    let session = SessionId::from(args.next().ok_or(usage)?.as_str());
    let mut now = chrono::Utc::now();
    let mut assume_survived = false;
    let mut quiet = 30usize;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--at" => {
                let value = args.next().ok_or(usage)?;
                now = chrono::DateTime::parse_from_rfc3339(&value)
                    .map_err(|e| format!("{flag}: {e}"))?
                    .with_timezone(&chrono::Utc)
            }
            "--assume-survived" => assume_survived = true,
            "--quiet" => {
                let value = args.next().ok_or(usage)?;
                quiet = value.parse().map_err(|e| format!("{flag}: {e}"))?
            }
            other => return Err(format!("unknown flag {other}; {usage}")),
        }
    }

    let options = SqliteConnectOptions::from_str(&path)
        .map_err(|e| format!("{path}: {e}"))?
        .read_only(true);
    let sqlite = SqliteStore::new(options);
    let mut snapshot = sqlite
        .load_session(&session)
        .await
        .map_err(|e| e.to_string())?;
    if snapshot.concepts.is_empty() {
        return Err(format!("session {session:?} has no concepts in {path}"));
    }
    let raised = if assume_survived {
        let mut n = 0;
        for c in snapshot.concepts.iter_mut() {
            if c.gc_survived < 3 {
                c.gc_survived = 3;
                n += 1;
            }
        }
        n
    } else {
        0
    };
    // The read-only SQLite store keeps its stored survival counts; with
    // --assume-survived the explanation must read the raised ones, which only
    // the graph carries (Stage 1 reads `gc_survived` from the concept row the
    // graph holds, never from the store).
    let resource = |t: ConceptType| {
        if t == ConceptType::Resource {
            "Resource"
        } else {
            "non-Resource"
        }
    };
    println!(
        "session={session} at={} concepts={} interactions={} edges={} assume_survived={assume_survived} raised={raised}",
        now.to_rfc3339(),
        snapshot.concepts.len(),
        snapshot.interactions.len(),
        snapshot.edges.len()
    );
    let mut start: BTreeMap<String, usize> = BTreeMap::new();
    for c in &snapshot.concepts {
        *start
            .entry(format!("{:?}/{:?}", c.canonization_status, c.concept_type))
            .or_default() += 1;
    }
    println!("starting status/type: {start:?}");

    for policy in [PromotionPolicy::Swarm, PromotionPolicy::Solo] {
        let params = EvalParams {
            promotion_policy: policy,
            ..EvalParams::default()
        };
        println!("\n== {policy} ==");

        // 1. First evaluation, unbatched, through the real SQLite queries.
        let graph = Graph::from_snapshot(snapshot.clone()).map_err(|e| e.to_string())?;
        let mut eligible: BTreeMap<String, usize> = BTreeMap::new();
        let mut to_canonical = Vec::new();
        for c in graph.concepts() {
            let Some(inputs) = admission_inputs(&graph, c.id, &params, now) else {
                continue;
            };
            let a = explain_admission(&sqlite, &session, &inputs, &params, now)
                .await
                .map_err(|e| e.to_string())?;
            if let (true, Some(next)) = (a.eligible, a.next) {
                *eligible
                    .entry(format!(
                        "{next:?} {} {:?}",
                        resource(c.concept_type),
                        c.concept_type
                    ))
                    .or_default() += 1;
                if next == CanonizationStatus::Canonical {
                    to_canonical.push(format!("{} {:?}", c.id, c.concept_type));
                }
            }
        }
        println!("first evaluation, eligible next hop (unbatched): {eligible:?}");
        for line in &to_canonical {
            println!("WOULD TURN CANONICAL: {line}");
        }

        // 2 and 3. The real evaluator on an in-memory copy.
        let memory = MemoryStore::new();
        let mut batch = MutationBatch::new();
        for i in &snapshot.interactions {
            batch.push(Mutation::UpsertNode {
                node: Node::Interaction(i.clone()),
            });
        }
        for c in &snapshot.concepts {
            batch.push(Mutation::UpsertNode {
                node: Node::Concept(c.clone()),
            });
        }
        for e in &snapshot.edges {
            batch.push(Mutation::UpsertEdge { edge: e.clone() });
        }
        memory
            .flush(&batch, None)
            .await
            .map_err(|e| e.to_string())?;
        let graph = RwLock::new(graph);
        let (tx, _rx) = lambo::daemon::events::event_channel();
        let mut ev = Evaluator::new();
        let mut at = now;
        let mut idle = 0;
        let mut cycles = 0;
        let mut hops: BTreeMap<String, usize> = BTreeMap::new();
        loop {
            let scores = {
                let g = graph.read();
                ScoreTable {
                    epoch: g.epoch(),
                    ranked: lambo::daemon::score::rescore(&g, &ScoringWeights::default()),
                }
            };
            let out = ev
                .eval_cycle(&graph, &memory, &scores, &tx, &params, at, None)
                .await
                .map_err(|e| e.to_string())?;
            cycles += 1;
            let g = graph.read();
            for e in out.transitions() {
                let ty = match g.node(e.node_id) {
                    Some(Node::Concept(c)) => resource(c.concept_type),
                    _ => "?",
                };
                *hops
                    .entry(format!("{:?}->{:?} {ty}", e.from_status, e.to_status))
                    .or_default() += 1;
            }
            if cycles == 1 {
                println!("first cycle (batch {}): {hops:?}", params.batch_size);
            }
            drop(g);
            at += chrono::Duration::seconds(61);
            if out.is_empty() {
                idle += 1;
                if idle >= quiet {
                    break;
                }
            } else {
                idle = 0;
            }
            if cycles >= 5_000 {
                return Err("no fixed point within 5000 cycles".into());
            }
        }
        let g = graph.read();
        let mut end: BTreeMap<String, usize> = BTreeMap::new();
        let mut canonical = Vec::new();
        for c in g.concepts() {
            if c.canonization_status != CanonizationStatus::None {
                *end.entry(format!("{:?}/{:?}", c.canonization_status, c.concept_type))
                    .or_default() += 1;
            }
            if c.canonization_status == CanonizationStatus::Canonical {
                canonical.push(format!(
                    "{} {:?} blast={:?}",
                    c.id, c.concept_type, c.blast_radius
                ));
            }
        }
        println!("fixed point after {cycles} cycles, all hops: {hops:?}");
        println!("fixed point status/type (above None): {end:?}");
        for line in canonical {
            println!("CANONICAL AT FIXED POINT: {line}");
        }
    }
    Ok(())
}
