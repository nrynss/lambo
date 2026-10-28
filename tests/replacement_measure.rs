//! #198 part c: the replacement-candidate list's measured recall and noise
//! on the #211 pairs, in CI.
//!
//! Runs the same loop as `cargo run --example replacement_measure` (this
//! file includes the example) over the committed pairs
//! (`fixtures/eval/pairs.json`) and BGE-M3 cosines
//! (`evidence/issue-198-trigger/cosines-bge-m3-self.json`), and pins the
//! counts at the shipped floor (the merge threshold, 0.85 under BGE-M3)
//! and the 0.82 row the floor was weighed against. A change to the
//! candidate predicate, the identifier guard, the threshold or the pairs
//! that moves them fails here; re-measure and update
//! `evidence/issue-198c-candidates/`.

#[path = "../examples/replacement_measure.rs"]
#[allow(dead_code)]
mod measure;

use measure::Count;

fn count(t: &std::collections::BTreeMap<(String, String), Count>, split: &str, rel: &str) -> Count {
    t[&(split.to_string(), rel.to_string())]
}

#[test]
fn the_shipped_floor_holds_its_measured_counts() {
    let (pairs, cosines) = measure::default_paths();
    let rows = measure::rows(&pairs, &cosines);
    assert_eq!(rows.len(), 147, "in-regime pairs");

    let shipped = measure::tally(&rows, 0.85);
    let c = |listed, of| Count { listed, of };
    assert_eq!(count(&shipped, "dev", "supersedes"), c(37, 43));
    assert_eq!(count(&shipped, "dev", "distinct_entity"), c(34, 47));
    assert_eq!(count(&shipped, "held_out", "supersedes"), c(25, 27));
    assert_eq!(count(&shipped, "held_out", "distinct_entity"), c(23, 30));

    let lower = measure::tally(&rows, 0.82);
    assert_eq!(count(&lower, "dev", "supersedes"), c(39, 43));
    assert_eq!(count(&lower, "held_out", "supersedes"), c(25, 27));

    // Every supersession missed at the shipped floor is a reworded one
    // below it, never a merge or a type refusal.
    for r in rows.iter().filter(|r| r.relation == "supersedes") {
        if !measure::listed(r, 0.85) {
            assert!(r.cosine < 0.85, "{} at {}", r.id, r.cosine);
        }
    }
}
