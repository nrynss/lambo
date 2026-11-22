//! The differential oracle (#291): the Lean models in `formal/LamboProofs/`
//! are executable, so their definitions are the reference.
//!
//! `lake exe oracle` (`formal/OracleExport/`) evaluates each covered Lean
//! definition over a fixed, deterministic set of generated inputs and writes
//! inputs and outputs to `formal/oracle/<Model>.json`. Each test here reads
//! one model's file (`include_str!`), rebuilds every input as the Rust types
//! the modelled function takes, calls the real function and asserts the
//! output the Lean computed. Nothing in the comparison is transcribed from
//! the model by hand, unlike the property tests (`src/**/proptests.rs`),
//! whose oracles (`spec_tie_key`, `spec_fits`, ...) restate the Lean in Rust.
//!
//! Where a model leaves a parameter abstract (a key order, a stemmer, an
//! estimator), the exporter instantiates it with the behaviour the Rust
//! documents; each file's `notes` say how, and so does each test's module
//! doc. A parameter that is itself Rust logic the model does not define is
//! not reimplemented in Lean: the inputs are restricted to where it is
//! trivial, and the test checks that restriction on the Rust side first
//! (`precondition`), so a broken instantiation fails as such and not as a
//! model mismatch.
//!
//! The vectors are checked into the repository; `scripts/formal/check-oracle.sh`
//! regenerates them and fails if they are stale. See formal/README.md,
//! "Differential oracle".

use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

mod batch_queue;
mod canon;
mod canonical;
mod expand;
mod fit;
mod fusion;
mod order;
mod score;
mod top_k;

/// One model's vector file, as `OracleExport.Gen.Vectors.render` writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    model: String,
    lean: String,
    #[allow(dead_code)]
    generator: String,
    #[allow(dead_code)]
    seed: u64,
    #[allow(dead_code)]
    notes: Vec<String>,
    /// Fixed data the cases share, by name.
    tables: serde_json::Map<String, Value>,
    functions: Vec<Function>,
}

/// One modelled function's cases.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Function {
    lean: String,
    rust: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    input: Value,
    output: Value,
}

impl Vectors {
    /// Parse a vector file and check that it is `model`'s and that its
    /// functions are exactly `lean_fns`, so a function the exporter adds
    /// without a Rust replay (or a replay whose function was dropped) fails.
    fn load(model: &str, json: &str, lean_fns: &[&str]) -> Vectors {
        let v: Vectors = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("formal/oracle/{model}.json does not parse: {e}"));
        assert_eq!(v.model, model, "formal/oracle/{model}.json names another model");
        assert_eq!(v.lean, format!("formal/LamboProofs/{model}.lean"));
        let mut have: Vec<&str> = v.functions.iter().map(|f| f.lean.as_str()).collect();
        let mut want = lean_fns.to_vec();
        have.sort_unstable();
        want.sort_unstable();
        assert_eq!(
            have, want,
            "formal/oracle/{model}.json: the functions in the file are not the ones this test replays"
        );
        v
    }

    /// The cases of the Lean function `lean`, each replayed by `run`. Every
    /// case is run; a mismatch names the model, the function, the case index
    /// and the input, with both outputs.
    fn replay<I, O>(&self, lean: &str, run: impl Fn(&I) -> O)
    where
        I: DeserializeOwned,
        O: DeserializeOwned + PartialEq + Debug,
    {
        let f = self
            .functions
            .iter()
            .find(|f| f.lean == lean)
            .unwrap_or_else(|| panic!("{}: no cases for {lean}", self.model));
        assert!(!f.cases.is_empty(), "{}: {lean} has no cases", self.model);
        let mut failures = Vec::new();
        for (i, case) in f.cases.iter().enumerate() {
            let input: I = serde_json::from_value(case.input.clone()).unwrap_or_else(|e| {
                panic!("{} {lean} case {i}: input does not parse: {e}", self.model)
            });
            let want: O = serde_json::from_value(case.output.clone()).unwrap_or_else(|e| {
                panic!("{} {lean} case {i}: output does not parse: {e}", self.model)
            });
            let got = run(&input);
            if got != want {
                failures.push(format!(
                    "case {i}\n  input: {}\n  Lean:  {want:?}\n  Rust:  {got:?}",
                    case.input
                ));
            }
        }
        if !failures.is_empty() {
            panic!(
                "{model}: Rust {rust} disagrees with Lean {lean} (formal/LamboProofs/{model}.lean) \
                 on {n} of {total} cases of formal/oracle/{model}.json; the first:\n{first}",
                model = self.model,
                rust = f.rust,
                n = failures.len(),
                total = f.cases.len(),
                first = failures
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    /// The shared table `name`.
    fn table<T: DeserializeOwned>(&self, name: &str) -> T {
        let t = self
            .tables
            .get(name)
            .unwrap_or_else(|| panic!("{}: no table {name}", self.model));
        serde_json::from_value(t.clone())
            .unwrap_or_else(|e| panic!("{}: table {name} does not parse: {e}", self.model))
    }

    /// Every input of `lean`'s cases, for a precondition check.
    fn inputs<I: DeserializeOwned>(&self, lean: &str) -> Vec<I> {
        self.functions
            .iter()
            .filter(|f| f.lean == lean)
            .flat_map(|f| &f.cases)
            .map(|c| serde_json::from_value(c.input.clone()).expect("input parses"))
            .collect()
    }
}

/// `formal/oracle/<name>.json`. The path is relative to the invoking file,
/// so this is for the submodules in `src/formal_oracle_tests/` only.
macro_rules! vectors {
    ($name:literal) => {
        include_str!(concat!("../../formal/oracle/", $name, ".json"))
    };
}
use vectors;

/// `std::cmp::Ordering` as the vectors spell it.
fn ord_char(o: std::cmp::Ordering) -> char {
    match o {
        std::cmp::Ordering::Less => '<',
        std::cmp::Ordering::Equal => '=',
        std::cmp::Ordering::Greater => '>',
    }
}

/// A comparator's table over `xs`, as `OracleExport.Gen.cmpTable` writes it.
fn cmp_table<T>(xs: &[T], cmp: impl Fn(&T, &T) -> std::cmp::Ordering) -> Vec<String> {
    xs.iter()
        .map(|a| xs.iter().map(|b| ord_char(cmp(a, b))).collect())
        .collect()
}

/// A precondition on the inputs, checked before the replay: it states how
/// the exporter instantiated a parameter the model leaves abstract, so a
/// failure here is a broken instantiation, not a model mismatch.
#[track_caller]
fn precondition(model: &str, ok: bool, what: impl FnOnce() -> String) {
    assert!(
        ok,
        "{model}: the oracle's instantiation does not hold on the Rust side: {}",
        what()
    );
}
