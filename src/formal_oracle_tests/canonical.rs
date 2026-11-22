//! `formal/oracle/Canonical.json`: Lean `Canonical.canonicalKeyF`,
//! `canonicalKey` and `nameKeyF`, with the model's tables `stop0` and
//! `stem0`, against `canonical_key`, `unfiltered_canonical_key`
//! (`src/graph/canonical.rs`) and `name_key` (`src/graph/graph/names.rs`).
//!
//! The model leaves the stemmer abstract and the real one (`rust-stemmers`,
//! Snowball English) is not reimplemented in Lean: every input stems only
//! words of the model's `stemTable` (`tables.stem0`), and this test first
//! checks the real stemmer against that table. `name_key`'s `check_size`
//! and UUID parse are parameters too; the name inputs pass the first and are
//! never UUIDs (checked).

use std::collections::HashMap;

use rust_stemmers::{Algorithm, Stemmer};

use super::{precondition, vectors, Vectors};
use crate::graph::canonical::{canonical_key, unfiltered_canonical_key};
use crate::graph::names::name_key;
use crate::surface::validate::check_size;

/// The model this module replays (`formal/oracle/Canonical.json`).
pub(super) const MODEL: &str = "Canonical";

/// A synonym callback over `table`, shaped as `canonical_key` takes it.
fn lookup(table: &'static HashMap<String, String>) -> impl Fn(&str) -> Option<&str> + Copy {
    move |raw| table.get(raw).map(String::as_str)
}

/// Replay this model's vectors; run by `super::oracle`, not a test of its
/// own, so it cannot be ignored or gated alone.
pub(super) fn replay() {
    let v = Vectors::load(MODEL, vectors!("Canonical"));

    let stemmer = Stemmer::create(Algorithm::English);
    for (word, stem) in v.table::<Vec<(String, String)>>("stem0") {
        let real = stemmer.stem(&word);
        precondition("Canonical", real == stem, || {
            format!("Lean stem0 stems {word:?} to {stem:?}, rust-stemmers to {real:?}")
        });
    }
    for name in v.inputs::<String>("Lambo.Canonical.nameKeyF") {
        precondition("Canonical", check_size("name", &name).is_ok(), || {
            format!("name input {name:?} fails check_size (Lean sizeOk is true)")
        });
        precondition(
            "Canonical",
            uuid::Uuid::parse_str(name.trim()).is_err(),
            || format!("name input {name:?} is a UUID (Lean isUuid is false)"),
        );
    }

    // Leaked so the lookup can return a borrow that outlives its argument,
    // as `canonical_key`'s `Fn(&str) -> Option<&str>` requires.
    let synonyms: &'static HashMap<String, String> = Box::leak(Box::new(
        v.table::<Vec<(String, String)>>("synonyms")
            .into_iter()
            .collect(),
    ));
    let syn = lookup(synonyms);

    v.replay("Lambo.Canonical.canonicalKeyF", |s: &String| {
        canonical_key(s, syn)
    });
    v.replay("Lambo.Canonical.canonicalKey", |s: &String| {
        unfiltered_canonical_key(s, syn)
    });
    v.replay("Lambo.Canonical.nameKeyF", |s: &String| name_key(s).ok());
}
