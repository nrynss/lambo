//! #198: the suspected-supersession trigger's measured precision, in CI.
//!
//! Runs the same loop as `cargo run --example suspected_measure` (this file
//! includes the example) over the committed #211 pairs
//! (`fixtures/eval/pairs.json`) and BGE-M3 cosines
//! (`evidence/issue-198-trigger/cosines-bge-m3-self.json`), and pins the
//! shipped lexicon's per-split counts. A change to the trigger, the
//! identifier guard, `PREDICATE_CUES` or the pairs that moves them fails
//! here; re-measure (on dev, never held-out) and update the evidence.

#[path = "../examples/suspected_measure.rs"]
#[allow(dead_code)]
mod measure;

#[test]
fn the_shipped_lexicon_holds_its_measured_precision() {
    let (pairs, cosines) = measure::default_paths();
    let (rows, _) = measure::rows(&pairs, &cosines);
    let shipped = |r: &measure::Row| r.verdict_shipped.suspected();

    let dev = measure::tally(&rows, "dev", shipped);
    assert_eq!((dev.tp, dev.fp), (35, 1), "dev TP/FP");
    assert_eq!(dev.false_positives, vec!["p-198-s29".to_string()]);

    let held_out = measure::tally(&rows, "held_out", shipped);
    assert_eq!((held_out.tp, held_out.fp), (25, 1), "held-out TP/FP");
    assert_eq!(held_out.false_positives, vec!["p-198-s30".to_string()]);
    assert_eq!(held_out.positives, 25, "held-out recall 25/25");
    assert!(held_out.missed.is_empty(), "{:?}", held_out.missed);
}
