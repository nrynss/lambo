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

use std::cell::RefCell;
use std::collections::BTreeSet;
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

/// The models a module here replays, one per file in `formal/oracle/`.
/// Each name is the module's own `MODEL`, which its test loads, so a name
/// here always has a replay; [`every_vector_file_is_replayed`] checks the
/// directory holds exactly these files.
const REPLAYED: [&str; 9] = [
    batch_queue::MODEL,
    canon::MODEL,
    canonical::MODEL,
    expand::MODEL,
    fit::MODEL,
    fusion::MODEL,
    order::MODEL,
    score::MODEL,
    top_k::MODEL,
];

/// A vector file no module replays (a model added to the exporter without a
/// Rust test), or a module whose file is gone, fails here.
#[test]
fn every_vector_file_is_replayed() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/formal/oracle");
    let files: BTreeSet<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .map(|entry| entry.expect("a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    let replayed: BTreeSet<String> = REPLAYED.iter().map(|m| format!("{m}.json")).collect();
    assert_eq!(
        files, replayed,
        "formal/oracle/ must hold exactly one vector file per model that src/formal_oracle_tests/ \
         replays (REPLAYED): add a module for a new file, or regenerate"
    );
}

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
    /// The functions [`Vectors::replay`] (or [`Vectors::skip`]) has handled,
    /// checked when the file is dropped.
    #[serde(skip)]
    handled: RefCell<BTreeSet<String>>,
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
    /// Parse a vector file and check that it is `model`'s. Every function in
    /// it must then be replayed (or skipped, with a reason) before it is
    /// dropped, so a function the exporter adds without a Rust replay fails,
    /// and a replay of a function the file lacks fails too.
    fn load(model: &str, json: &str) -> Vectors {
        let v: Vectors = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("formal/oracle/{model}.json does not parse: {e}"));
        assert_eq!(
            v.model, model,
            "formal/oracle/{model}.json names another model"
        );
        assert_eq!(v.lean, format!("formal/LamboProofs/{model}.lean"));
        assert!(
            REPLAYED.contains(&model),
            "{model} is not in REPLAYED (src/formal_oracle_tests.rs)"
        );
        v
    }

    /// Mark `lean` as deliberately not replayed in this build, for `why`
    /// (a function whose Rust needs a feature this build lacks).
    #[allow(dead_code)] // used only without some features
    fn skip(&self, lean: &str, why: &str) {
        assert!(!why.is_empty());
        assert!(
            self.functions.iter().any(|f| f.lean == lean),
            "{}: no function {lean} to skip",
            self.model
        );
        self.handled.borrow_mut().insert(lean.to_owned());
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
        self.handled.borrow_mut().insert(lean.to_owned());
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

impl Drop for Vectors {
    /// Every function in the file was replayed or skipped. Not checked while
    /// a replay is already failing.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let handled = self.handled.borrow();
        let missed: Vec<&str> = self
            .functions
            .iter()
            .map(|f| f.lean.as_str())
            .filter(|f| !handled.contains(*f))
            .collect();
        assert!(
            missed.is_empty(),
            "formal/oracle/{}.json: no test replays {missed:?}",
            self.model
        );
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
