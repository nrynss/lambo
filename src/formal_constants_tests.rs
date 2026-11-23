//! The constants the Lean models in `formal/LamboProofs/` hard-code, pinned
//! to the Rust they were read from (#291, step 3).
//!
//! The Lean models are transcriptions: a threshold, weight or bound in a
//! model is a literal copied from the Rust. Nothing makes the two move
//! together, so each test here reads its model's source (`include_str!`)
//! and asserts the Rust against the value the model states. Each value has
//! one copy in this file's comparisons, the model's:
//!
//! * a scalar definition (`def windowMax : Nat := 20`), a structure field's
//!   default (read within that structure), a field of a record or
//!   anonymous-constructor literal, or a list of string literals is parsed
//!   out of the model ([`Lean::def`], [`Lean::field`], [`lean::Val`]). It must be a
//!   bare literal of the stated type: `def windowMax : Nat := 20 * 2` fails
//!   rather than reads 20;
//! * a definition computed from others or from its parameters (a `Nat`
//!   formula: `def window (topK : Nat) : Nat := min windowMax (4 * topK)`)
//!   is evaluated ([`Lean::apply`]) and compared with the Rust constant or
//!   function, over a range of inputs for a function;
//! * a definition whose shape matters but that is not a `Nat` formula (a
//!   `Bool` guard, a marker built from strings) is matched as a token
//!   sequence with a `$` hole for each literal the Rust is compared against
//!   ([`Lean::captures`]); the same holds a formula's literal when the Rust
//!   names it as a constant (`WINDOW_PER_SLOT`). The tokens outside the
//!   holes (a guard's operators and structure) are a second copy, held in
//!   this file: a model and this file edited together, `3 ≤ n` to `3 < n`
//!   in both, go unnoticed. That is the known gap; the values themselves
//!   are read from the model;
//! * a table over an inductive (a `match` by constructor) is evaluated per
//!   constructor. The test names each Rust variant's Lean constructor
//!   ([`lean_ctors`]), an exhaustive `match`, so a Rust variant added later
//!   stops these tests compiling, and the names must cover the Lean
//!   inductive exactly, so a Lean constructor added later fails them;
//! * two pins are prose, a model's doc comment stating a Rust behaviour,
//!   matched as text ([`Lean::doc_states`]), the numbers it states read out
//!   ([`Lean::doc_captures`]).
//!
//! The reader (lexer, declaration boundary, parsers, evaluator) and its own
//! tests are in [`lean`].
//!
//! Comments are dropped before a definition is read, so an old value left in
//! a comment neither matches nor hides a change. A definition or shape must
//! occur once and end where its declaration ends (the next declaration, the
//! next field of its structure, or the end of the file), so a value extended
//! in place (`1000` to `1000 * 2`) fails. Whitespace between tokens does not
//! matter; inside a string literal it does. A Rust-side failure names the
//! Lean file and definition; so does a Lean-side one (a definition gone,
//! reshaped or stated twice). Each Lean definition names the test that pins
//! it.
//!
//! One test per Lean file, named after it. Fixed-point models keep their
//! scale: a Lean integer `n` at scale `s` is asserted as `n as f64 / s as
//! f64`, which is the same correctly rounded `f64` as the Rust literal, so
//! the comparison is exact. Durations the models keep in milliseconds or
//! seconds are compared through `Duration::from_millis` or `from_secs`.
//! Where a Rust value needs a feature, only that half of the test is gated;
//! the Lean half always runs.
//!
//! Models with no hard-coded Rust value (`Order`, `Expand`, `Cite`,
//! `Names`, `Lease`, `Reserve`, `Contract`) have no test here. `Audit.lean`
//! is not a model: it prints the axioms the others' theorems rest on.

use std::time::Duration;

use crate::config::{Config, ScoringWeights};
use crate::daemon::hotlist::{default_contradiction_max, Condition, HOT_LIST_MAX};
use crate::types::{ConceptType, EdgeType};

/// `formal/LamboProofs/<name>.lean`, as `(name, source)`.
macro_rules! model {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../formal/LamboProofs/", $name, ".lean")),
        )
    };
}

/// A Rust enum's Lean constructors, written once: `(ctor, variants)`.
/// `ctor` is an exhaustive `match` with no wildcard arm, so a variant added
/// to the Rust enum stops the test compiling until its Lean constructor is
/// named. `variants` is the list of that match's arms, so it holds every
/// variant by construction (a repeated arm is an unreachable pattern).
macro_rules! lean_ctors {
    ($ty:ident { $($variant:ident => $ctor:literal),+ $(,)? }) => {
        (
            (|v: $ty| -> &'static str {
                match v {
                    $($ty::$variant => $ctor),+
                }
            }) as fn($ty) -> &'static str,
            [$($ty::$variant),+],
        )
    };
}

/// `assert_eq!(rust, pin.value)`, naming the Rust expression and the Lean
/// definition the value was read from.
macro_rules! assert_pin {
    ($rust:expr, $pin:expr $(,)?) => {{
        let pin = &$pin;
        assert_eq!(
            $rust,
            pin.value,
            "`{}` no longer matches {}: change one to match the other",
            stringify!($rust),
            pin.at
        );
    }};
}

mod lean;

use lean::{millis, nat, Lean, Nat};

// ---------------------------------------------------------------------------
// One test per model
// ---------------------------------------------------------------------------

#[test]
fn score() {
    use crate::daemon::score::{
        MAX_BONUS, MAX_CONCEPT_MODIFIER, MAX_EDGE_BONUS, MIN_CONCEPT_MODIFIER, MIN_RECENCY_SPAN,
    };

    // Score.lean works at scale 10³ (a composite at 10⁶).
    let lean = Lean::new(model!("Score"));

    let w = ScoringWeights::default();
    let dflt = lean.def("defaultWeights", "Weights");
    let weight = |field: &str| dflt.record(field).shape("some $").int(0).scaled(1000);
    assert_pin!(w.recency, weight("recency"));
    assert_pin!(w.frequency, weight("frequency"));
    assert_pin!(w.session_activity, weight("sessionActivity"));
    assert_pin!(w.density, weight("density"));

    let bonus = lean.captures(
        "def bonusAndModifier (d : Dims) : Int :=
           clampAdditive $ $ d.edgeTypeBonus + clampAdditive $ $ d.conceptTypeModifier",
    );
    // `score_bonus`'s `clamp_additive(…, 0.0, MAX_EDGE_BONUS)`.
    assert_pin!(0.0, bonus.int(0).scaled(1000));
    assert_pin!(MAX_EDGE_BONUS, bonus.int(1).scaled(1000));
    assert_pin!(MIN_CONCEPT_MODIFIER, bonus.int(2).scaled(1000));
    assert_pin!(MAX_CONCEPT_MODIFIER, bonus.int(3).scaled(1000));
    assert_eq!(MAX_BONUS, MAX_EDGE_BONUS + MAX_CONCEPT_MODIFIER);

    // `finite_or_zero`'s `x.clamp(0.0, 1.0 + MAX_BONUS)`.
    let finite = lean.captures("def finiteOrZero (x : Int) : Int := clamp $ $ x");
    assert_pin!(0.0, finite.int(0).scaled(1_000_000));
    assert_pin!(1.0 + MAX_BONUS, finite.int(1).scaled(1_000_000));

    assert_pin!(
        MIN_RECENCY_SPAN,
        lean.def("minRecencySpan", "Int").int().to::<i64>()
    );
}

#[test]
fn canon() {
    use crate::canon::formal_pins::{
        MIN_BLAST_RADIUS, MIN_COVERAGE, MIN_DEPENDENT_INTERACTIONS, MIN_DISTINCT, MIN_GC_SURVIVED,
    };

    let lean = Lean::new(model!("Canon"));

    let stage1 = lean.captures(
        "def stage1Passes (gcSurvived dependentInteractions : Nat) : Bool :=
           decide ($ ≤ gcSurvived) && decide ($ ≤ dependentInteractions)",
    );
    assert_pin!(MIN_GC_SURVIVED, stage1.int(0).to::<i32>());
    assert_pin!(MIN_DEPENDENT_INTERACTIONS, stage1.int(1).to::<usize>());

    // Coverage ≥ num/den, cross-multiplied.
    let stage2 = lean.captures(
        "def stage2Passes (distinct coverageNum coverageDen : Nat) : Bool :=
           decide ($ ≤ distinct) && decide ($ * coverageDen ≤ $ * coverageNum)",
    );
    assert_pin!(MIN_DISTINCT, stage2.int(0).to::<u64>());
    let den = stage2.int(2).value;
    assert_pin!(MIN_COVERAGE, stage2.int(1).scaled(den));

    // `5 < blast`, strict.
    let stage3 = lean.captures(
        "def stage3Passes (lastDemotion : Option Int) (cooldown now : Int) (blast : Nat) : Bool :=
           !inCooldown lastDemotion cooldown now && decide ($ < blast)",
    );
    assert_pin!(MIN_BLAST_RADIUS, stage3.int(0).to::<u64>());
}

#[test]
fn gc() {
    let lean = Lean::new(model!("Gc"));

    // i32Max is `i32::MAX`, where `gc_survived` saturates: the type below
    // pins that the count is an `i32`.
    let _survivors: fn(&crate::types::Concept) -> i32 = |c| c.gc_survived;
    assert_pin!(i128::from(i32::MAX), lean.def("i32Max", "Nat").nat());

    let counts = lean.table("EdgeType.countsTowardGcConnectivity", "EdgeType → Bool");
    let (ctor, listed) = lean_ctors!(EdgeType {
        Temporal => "temporal",
        Derives => "derives",
        CoOccurrence => "coOccurrence",
        Causal => "causal",
        Dependency => "dependency",
        Hierarchical => "hierarchical",
        Semantic => "semantic",
        Contradicts => "contradicts",
        SuspectedSupersedes => "suspectedSupersedes",
    });
    counts.check_names(&listed.map(ctor));
    for ty in listed {
        assert_pin!(
            ty.counts_toward_gc_connectivity(),
            counts.at(ctor(ty)).bool()
        );
    }
}

#[test]
fn top_k() {
    use crate::recall::redundancy::{window, WINDOW_MAX, WINDOW_PER_SLOT};

    let lean = Lean::new(model!("TopK"));
    assert_pin!(WINDOW_MAX, lean.def("windowMax", "Nat").nat().to::<usize>());
    let per_slot = lean
        .captures("def window (topK : Nat) : Nat := min windowMax ($ * topK)")
        .int(0)
        .to::<usize>();
    assert_pin!(WINDOW_PER_SLOT, per_slot);
    for top_k in 0..10 {
        assert_pin!(
            window(top_k),
            lean.apply("window", &[nat(top_k)]).to::<usize>()
        );
    }
}

#[test]
fn fit() {
    use crate::recall::format::fit_formal_pins::{truncation_marker, ELLIPSIS, SEPARATOR};
    use crate::recall::format::{default_token_count, fit_blocks, ContextBlock};

    let lean = Lean::new(model!("Fit"));

    // usizeMax is `usize::MAX` on a 64-bit target, where `fits` saturates.
    let usize_max = lean.apply("usizeMax", &[]);
    if cfg!(target_pointer_width = "64") {
        assert_pin!(
            u128::try_from(usize::MAX).expect("usize fits u128"),
            usize_max.to::<u128>()
        );
    }

    // defaultG: ceil(bytes / 3.5), the Rust's `(2 * bytes).div_ceil(7)`.
    for bytes in 0..512 {
        assert_pin!(
            default_token_count(&"y".repeat(bytes)),
            lean.apply("defaultG", &[nat(bytes)]).to::<usize>()
        );
    }

    // rustEnv: `⟨est, sep, ell, nl, ws⟩`.
    assert_eq!(
        lean.fields("Env"),
        ["est", "sep", "ell", "nl", "ws"],
        "{}",
        lean.at("Env")
    );
    let env = lean.captures(
        "def rustEnv (est : List Char → Nat) : Env Char := ⟨est, [$, $], [$], [$], rustWs⟩",
    );
    let sep: String = [env.chr(0).value, env.chr(1).value].into_iter().collect();
    assert_eq!(SEPARATOR, sep, "{} (sep)", env.at);
    assert_pin!(ELLIPSIS, env.chr(2).map_to_string());
    let nl = env.chr(3);
    // The `nl` before the marker, read off a real truncation: the head ends
    // at the ellipsis and the marker follows `nl`.
    let content = "word ".repeat(60);
    let block = ContextBlock {
        text: content.clone(),
        content_len: content.len(),
        label_end: content.len(),
        inspect_id: "1a2b3c4d".to_string(),
        scope: None,
    };
    let fitted = fit_blocks(&[block], 40, default_token_count);
    assert!(fitted.truncation.is_some(), "{fitted:?}");
    let (head, marker) = fitted
        .context
        .rsplit_once(nl.value)
        .unwrap_or_else(|| panic!("{}: no marker line in {:?}", nl.at, fitted.context));
    assert!(
        head.len() > ELLIPSIS.len() && head.ends_with(ELLIPSIS),
        "{}: {head:?}",
        nl.at
    );

    // rustMarker, both forms, from the model's pieces.
    let pieces = lean.captures(
        "def rustMarker (inspectId : List Char) (scope : Option (List Char)) (n : Nat) : List Char :=
           $.toList ++ Nat.toDigits 10 n ++ $.toList ++
             inspectId ++
             (match scope with
              | none => []
              | some s => $.toList ++ s ++ [$]) ++
             $.toList",
    );
    let (open, mid, scope_open, scope_close, close) = (
        pieces.str(0).value,
        pieces.str(1).value,
        pieces.str(2).value,
        pieces.chr(3).value,
        pieces.str(4).value,
    );
    assert!(marker.starts_with(&open), "{}: {marker:?}", pieces.at);
    let lean_marker = |n: usize, id: &str, scope: Option<&str>| -> String {
        let scope = scope.map_or(String::new(), |s| format!("{scope_open}{s}{scope_close}"));
        format!("{open}{n}{mid}{id}{scope}{close}")
    };
    for n in [0, 7, 1234] {
        for scope in [None, Some("general")] {
            assert_eq!(
                truncation_marker(n, "1a2b3c4d", scope),
                lean_marker(n, "1a2b3c4d", scope),
                "{}",
                pieces.at
            );
        }
    }
}

#[test]
fn receipts() {
    use crate::writeq::{
        MAX_CONCURRENT_RECEIPT_WAITS, MAX_RETAINED_RECEIPTS, WRITE_QUEUE_LANE_MAX, WRITE_QUEUE_MAX,
    };

    let lean = Lean::new(model!("Receipts"));
    assert_pin!(
        MAX_RETAINED_RECEIPTS,
        lean.def("maxRetainedReceipts", "Nat").nat().to::<usize>()
    );
    assert_pin!(
        WRITE_QUEUE_MAX,
        lean.apply("writeQueueMax", &[]).to::<usize>()
    );
    assert_pin!(
        WRITE_QUEUE_LANE_MAX,
        lean.apply("writeQueueLaneMax", &[]).to::<usize>()
    );
    // The lane cap divides by the receipt waits.
    let lanes = lean
        .captures("def writeQueueLaneMax : Nat := writeQueueMax / $")
        .int(0)
        .to::<usize>();
    assert_pin!(MAX_CONCURRENT_RECEIPT_WAITS, lanes);
}

#[test]
fn fusion() {
    use crate::recall::fusion::{
        RecallFusion, DEFAULT_LEG_WEIGHT, DEFAULT_RECENT_WEIGHT, DEFAULT_RRF_K,
    };

    // The weights are at scale 10 (`10` for `1.0`), k a fraction kNum/kDen.
    let lean = Lean::new(model!("Fusion"));
    let fields = lean.fields("RrfConfig");
    let dflt = lean.def("rrfDefault", "RrfConfig");
    let den = dflt.anon(&fields, "kDen").nat().value;
    assert_pin!(DEFAULT_RRF_K, dflt.anon(&fields, "kNum").nat().scaled(den));
    assert_pin!(
        DEFAULT_LEG_WEIGHT,
        dflt.anon(&fields, "wKeyword").nat().scaled(10)
    );
    assert_pin!(
        DEFAULT_LEG_WEIGHT,
        dflt.anon(&fields, "wVector").nat().scaled(10)
    );
    assert_pin!(
        DEFAULT_RECENT_WEIGHT,
        dflt.anon(&fields, "wRecent").nat().scaled(10)
    );
    let d = RecallFusion::default();
    assert_eq!(d.k, DEFAULT_RRF_K);
    assert_eq!(d.keyword_weight, DEFAULT_LEG_WEIGHT);
    assert_eq!(d.vector_weight, DEFAULT_LEG_WEIGHT);
    assert_eq!(d.recent_weight, DEFAULT_RECENT_WEIGHT);
}

#[test]
fn confidence() {
    use crate::recall::candidates::KEYWORD_OVERFETCH;
    use crate::recall::confidence::{BASIS, KEYWORD_HALF_SCORE, KEYWORD_ONLY_BASIS};

    let lean = Lean::new(model!("Confidence"));
    // In units of 1.0.
    assert_pin!(
        KEYWORD_HALF_SCORE,
        lean.def("keywordHalfScore", "Int").int().scaled(1)
    );
    let overfetch = lean.captures(
        "def keywordLoop [DecidableEq ι] (limit : Nat) (ranked : List (ι × F64))
             (legs : List (ι × LegSlot)) : List (ι × LegSlot) :=
           (ranked.take (limit * $)).foldl (fun acc p => upsertKeyword p.1 (fun _ => some p.2) acc) legs",
    );
    assert_pin!(KEYWORD_OVERFETCH, overfetch.int(0).to::<usize>());

    let names = lean.table("Basis.name", "Basis → String");
    names.check_names(&["noisyOrV0", "keywordOnlyV0"]);
    assert_pin!(BASIS, names.at("noisyOrV0").string());
    assert_pin!(KEYWORD_ONLY_BASIS, names.at("keywordOnlyV0").string());
}

#[test]
fn canonical() {
    use crate::graph::canonical::formal_pins::STOPWORDS;

    let lean = Lean::new(model!("Canonical"));
    assert_pin!(
        STOPWORDS.to_vec(),
        lean.def("stopwords", "List String").strings()
    );
}

#[test]
fn dispatch() {
    use crate::recall::dispatch::formal_pins::STRUCTURAL_MARKERS;

    let lean = Lean::new(model!("Dispatch"));
    assert_pin!(
        STRUCTURAL_MARKERS.to_vec(),
        lean.def("markerStrings", "List String").strings()
    );
}

#[test]
fn supersession() {
    use crate::canon::formal_pins::{MIN_COVERAGE, MIN_DISTINCT};
    use crate::graph::suspected::replacement::REPLACEMENT_CANDIDATE_LIMIT;

    let suspected = Lean::new(model!("Supersession/Suspected"));
    assert_pin!(
        REPLACEMENT_CANDIDATE_LIMIT,
        suspected.def("limit", "Nat").nat().to::<usize>()
    );
    let lean = Lean::new(model!("Supersession/Pass"));
    // Stage 2's distinct origins.
    let since = lean.captures(
        "def stage2Since (ev : Evidence) (window since : Int) : Bool :=
           let c := counted ev since
           decide ($ ≤ (sortDedup (c.map (·.1.origin))).length) && coveragePasses ev window c",
    );
    assert_pin!(MIN_DISTINCT, since.int(0).to::<u64>());
    // Stage 2's coverage, `3 * sess ≤ 10 * span`.
    let coverage = lean.captures(
        "def coveragePasses (ev : Evidence) (window : Int) (c : List (Row × Int)) : Bool :=
           match c.map (·.2) with
           | [] => false
           | d :: ds =>
             let lo := ds.foldl min d
             let hi := ds.foldl max d
             let ext := ev.extent.getD (lo, hi)
             let sess := max 0 (ext.2 - ext.1)
             let span := max 0 (hi - lo)
             if sess ≤ 0 then true else decide ($ * min sess window ≤ $ * span)",
    );
    let den = coverage.int(1).value;
    assert_pin!(MIN_COVERAGE, coverage.int(0).scaled(den));

    // The header says `canonization_supersede` defaults off (the `*_off`
    // theorems rest on it).
    lean.doc_states(&[
        "The option defaults to off (`canonization_supersede: false` in `Config::default`,",
    ]);
    assert!(!Config::default().canonization_supersede);
}

#[test]
fn policy() {
    use crate::canon::formal_pins::{
        CANDIDATE_BAR, CANONICAL_BAR, DEPENDENT_INTERACTION_WEIGHT, HUMAN_CONFIRMED_WEIGHT,
        REVERT_WEIGHT, SESSION_SEPARATION, SESSION_WEIGHT, VENERABLE_BAR,
    };
    use crate::canon::PromotionPolicy;

    let sessions = Lean::new(model!("Policy/Sessions"));
    assert_pin!(
        SESSION_SEPARATION,
        sessions.def("sessionSeparation", "Nat").nat().secs()
    );
    let lean = Lean::new(model!("Policy/Score"));
    let weight = |name: &str| lean.def(name, "Int").int().scaled(1);
    assert_pin!(SESSION_WEIGHT, weight("sessionWeight"));
    assert_pin!(HUMAN_CONFIRMED_WEIGHT, weight("humanConfirmedWeight"));
    assert_pin!(
        DEPENDENT_INTERACTION_WEIGHT,
        weight("dependentInteractionWeight")
    );
    assert_pin!(REVERT_WEIGHT, weight("revertWeight"));
    // The bars are in tenths.
    let bar = |name: &str| lean.def(name, "Int").int().scaled(10);
    assert_pin!(CANDIDATE_BAR, bar("candidateBar10"));
    assert_pin!(VENERABLE_BAR, bar("venerableBar10"));
    assert_pin!(CANONICAL_BAR, bar("canonicalBar10"));

    // In tenths.
    let resistance = lean.table("ConceptType.resistance10", "ConceptType → Int");
    let (ctor, listed) = lean_ctors!(ConceptType {
        Entity => "entity",
        Logic => "logic",
        Constraint => "constraint",
        Resource => "resource",
        Observation => "observation",
    });
    resistance.check_names(&listed.map(ctor));
    for ty in listed {
        assert_pin!(
            ty.eviction_resistance(),
            resistance.at(ctor(ty)).int().scaled(10)
        );
    }

    let (ctor, listed) = lean_ctors!(PromotionPolicy {
        Swarm => "swarm",
        Solo => "solo",
    });
    lean.check_ctors("PromotionPolicy", &listed.map(ctor));
    assert_pin!(
        ctor(PromotionPolicy::default()),
        lean.def("PromotionPolicy.default", "PromotionPolicy")
            .ctor()
    );
}

#[test]
fn conflict() {
    use crate::daemon::conflict::CONFLICT_RECENCY_WINDOW;

    let lean = Lean::new(model!("Conflict/Detect"));
    // Seconds.
    assert_pin!(
        CONFLICT_RECENCY_WINDOW,
        lean.def("recencyWindow", "Option Nat")
            .shape("some $")
            .int(0)
            .secs()
    );
}

#[test]
fn hotlist() {
    let lean = Lean::new(model!("Hotlist/List"));
    assert_pin!(
        HOT_LIST_MAX,
        lean.def("hotListMax", "Nat").nat().to::<usize>()
    );
    for max in [0, 1, 2, 3, 999, HOT_LIST_MAX] {
        assert_pin!(
            default_contradiction_max(max),
            lean.apply("defaultCmax", &[nat(max)]).to::<usize>()
        );
    }

    let severity = lean.table("Cond.severity", "Cond → Nat");
    let (ctor, listed) = lean_ctors!(Condition {
        Conflict => "conflict",
        HighRiskModification => "highRisk",
        Drift => "drift",
        StaleSession => "stale",
        Contradiction => "contradiction",
    });
    severity.check_names(&listed.map(ctor));
    for cond in listed {
        assert_pin!(cond.severity(), severity.at(ctor(cond)).nat().to::<u8>());
    }
}

#[test]
fn drain() {
    use crate::writeq::{EMBEDDER_SICK_THRESHOLD, REPLAY_LOOKUP_TIMEOUT_LIMIT};

    let lean = Lean::new(model!("Drain/Base"));
    assert_pin!(
        EMBEDDER_SICK_THRESHOLD,
        lean.def("sickThreshold", "Nat").nat().to::<usize>()
    );
    assert_pin!(
        REPLAY_LOOKUP_TIMEOUT_LIMIT,
        lean.apply("lookupTimeoutLimit", &[]).to::<usize>()
    );
}

#[test]
fn flush() {
    use crate::store::flush::formal_pins::{
        next_backoff, BACKOFF_BASE, BACKOFF_CAP, RETAINED_BACKOFF,
    };

    // Milliseconds.
    let lean = Lean::new(model!("Flush/Retry"));
    assert_pin!(BACKOFF_BASE, lean.def("backoffBase", "Nat").nat().ms());
    assert_pin!(BACKOFF_CAP, lean.def("backoffCap", "Nat").nat().ms());
    assert_pin!(RETAINED_BACKOFF, lean.apply("retainedBackoff", &[]).ms());
    // The retry loop's doubling, up the ladder and past the cap.
    for b in [0, 1, 100, 200, 400, 6_399, 6_400, 10_000, 20_000] {
        assert_pin!(
            next_backoff(Duration::from_millis(b)),
            lean.apply("nextBackoff", &[Nat::from(b)]).ms()
        );
    }
}

#[test]
fn batch_queue() {
    let lean = Lean::new(model!("BatchQueue"));
    let cfg_fields = lean.fields("Cfg");
    let cfg = lean.def("Cfg.rust", "Cfg");
    let field = |name: &str| cfg.anon(&cfg_fields, name).nat().to::<usize>();
    let (max_batch, width, slack, streak) = (
        field("maxBatch"),
        field("width"),
        field("slack"),
        field("streak"),
    );
    let canary_len = lean
        .def("canary", "Txt")
        .anon(&lean.fields("Txt"), "len")
        .nat()
        .to::<usize>();
    // `len i ≤ 2 * len h + slack` joins the open chunk.
    let ratio = lean
        .captures(
            "def lcAux (c : Cfg) (len : Nat → Nat) (cur : List Nat) (h : Nat) :
                 List Nat → List (List Nat)
               | [] => [cur]
               | i :: is =>
                 if cur.length < c.width ∧ len i ≤ $ * len h + c.slack then lcAux c len (cur ++ [i]) h is
                 else cur :: lcAux c len [i] i is",
        )
        .int(0)
        .to::<usize>();

    #[cfg(feature = "embed-candle")]
    {
        use crate::embed::candle_formal_pins::{
            length_chunks, CANARY_TEXT, CHUNK_SLACK_BYTES, FORWARD_WIDTH, MAX_BATCH,
            MAX_SINGLE_STREAK,
        };

        assert_pin!(MAX_BATCH, max_batch);
        assert_pin!(FORWARD_WIDTH, width);
        assert_pin!(CHUNK_SLACK_BYTES, slack);
        assert_pin!(MAX_SINGLE_STREAK, streak);
        assert_pin!(CANARY_TEXT.len(), canary_len);
        // At two shortest lengths, so the ratio and the slack are both
        // pinned.
        for shortest in [10, 100] {
            let edge = ratio.value * shortest + slack.value;
            assert_eq!(
                length_chunks(&[shortest, edge]),
                vec![vec![0, 1]],
                "{}",
                ratio.at
            );
            assert_eq!(
                length_chunks(&[shortest, edge + 1]),
                vec![vec![0], vec![1]],
                "{}",
                ratio.at
            );
        }
    }
    #[cfg(not(feature = "embed-candle"))]
    let _ = (max_batch, width, slack, streak, canary_len, ratio);
}

#[test]
fn registry() {
    use crate::config::ServeConfig;
    use crate::mcp::serve::registry_formal_pins::{
        pinned_retry, unplaced_cap, ELECTION_RETRY, EVICT_MIN_IDLE, LEASE_RELEASE_GRACE,
        NEGATIVE_TTL, PINNED_RETRY, PREVIOUS_HANDLE_WAIT, UNPLACED_PER_PLACE,
    };

    let lean = Lean::new(model!("Registry"));

    // pinnedRetry (milliseconds): at the Rust's election retry, and swept
    // across the floor so both branches are compared.
    assert_pin!(
        PINNED_RETRY,
        lean.apply("pinnedRetry", &[millis(ELECTION_RETRY)]).ms()
    );
    for er in [0, 1, 150, 199, 200, 201, 999, 1000, 1001, 30_000] {
        assert_pin!(
            pinned_retry(Duration::from_millis(er)),
            lean.apply("pinnedRetry", &[Nat::from(er)]).ms()
        );
    }

    let per_place = lean
        .captures("def unplacedCap (maxAttached : Nat) : Nat := max (maxAttached * $) $")
        .int(0)
        .to::<usize>();
    assert_pin!(UNPLACED_PER_PLACE, per_place);
    for max_attached in [0, 1, 2, 16] {
        assert_pin!(
            unplaced_cap(max_attached),
            lean.apply("unplacedCap", &[nat(max_attached)])
                .to::<usize>()
        );
    }

    // cfgEx's fixed constants, in milliseconds.
    let cfg = lean.def("cfgEx", "Cfg");
    assert_pin!(ELECTION_RETRY, cfg.record("er").nat().ms());
    assert_pin!(PINNED_RETRY, lean.eval_val(&cfg.record("pr")).ms());
    assert_pin!(PREVIOUS_HANDLE_WAIT, cfg.record("wait").nat().ms());
    assert_pin!(EVICT_MIN_IDLE, cfg.record("minIdle").nat().ms());
    assert_pin!(NEGATIVE_TTL, cfg.record("ttl").nat().ms());

    // dropped_flight_fences_next's doc: the TTL and the release grace.
    let doc = lean.doc_captures(
        "delayed past `NEGATIVE_TTL` ($ s; the release itself is bounded by
         `LEASE_RELEASE_GRACE`, $ s,",
    );
    assert_pin!(NEGATIVE_TTL, doc.int(0).secs());
    assert_pin!(LEASE_RELEASE_GRACE, doc.int(1).secs());

    // cfgEx's example settings (not defaults): the pinned sessions,
    // `max_attached` (the argument of `cap := unplacedCap …`), the permits
    // and the idle timeout. Built as a `ServeConfig`, `validate()` accepts
    // them (non-zero bounds, no more pinned sessions than `max_attached`),
    // and the serve derives cfgEx's `places`, `permits` and `idle` from
    // them.
    let pinned = cfg.record("pinned").nats();
    let max_attached = cfg
        .record("cap")
        .shape("unplacedCap $")
        .int(0)
        .to::<usize>();
    let permits = cfg.record("permits").nat().to::<usize>();
    let idle = cfg.record("idle").nat().ms();
    let witness = ServeConfig {
        sessions: (0..pinned.value.len())
            .map(|i| format!("pinned{i}"))
            .collect(),
        max_attached: Some(max_attached.value),
        attach_concurrency: Some(permits.value),
        idle_detach_secs: Some(idle.value.as_secs()),
        ..ServeConfig::default()
    };
    witness.validate().unwrap_or_else(|e| {
        panic!(
            "{}: its example settings no longer pass `ServeConfig::validate`: {e}",
            cfg.at
        )
    });
    assert_pin!(witness.idle_detach(), idle);
    assert_pin!(witness.attach_concurrency(), permits);
    assert_pin!(
        witness.max_attached() - witness.sessions.len(),
        cfg.record("places").nat().to::<usize>()
    );
    assert_pin!(
        unplaced_cap(witness.max_attached()),
        lean.eval_val(&cfg.record("cap")).to::<usize>()
    );
}

#[test]
fn resolve() {
    use crate::config::DEFAULT_MAX_ATTACHED;
    use crate::embed::{EmbedderConfig, EmbedderKind};
    use crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES;

    let lean = Lean::new(model!("Resolve"));

    // The `EmbedderConfig` default width: defaultDim, and Raw.dim's default.
    let default_dim = lean.def("defaultDim", "Nat").nat().to::<usize>();
    assert_pin!(EmbedderConfig::default().dim, default_dim);
    assert_pin!(
        EmbedderConfig::default().dim,
        lean.field("Raw", "dim", "Nat").nat().to::<usize>()
    );
    assert_pin!(
        MAX_BEARER_CREDENTIAL_BYTES,
        lean.def("maxBearer", "Nat").nat().to::<usize>()
    );
    assert_pin!(
        DEFAULT_MAX_ATTACHED,
        lean.field("ServeCfg", "maxAttached", "Nat")
            .nat()
            .to::<usize>()
    );

    // Features.crdbWidth: the Cockroach migration's first `VECTOR(n)`, read
    // as `ddl_vector_dim` reads it.
    let ddl = include_str!("../migrations/cockroach/001_init.sql");
    let rest = &ddl[ddl.find("VECTOR(").expect("VECTOR(n)") + "VECTOR(".len()..];
    let width: usize = rest[..rest.find(')').expect(")")]
        .trim()
        .parse()
        .expect("n");
    assert_pin!(
        width,
        lean.field("Features", "crdbWidth", "Nat")
            .nat()
            .to::<usize>()
    );

    // EmbedderKind.implemented: every kind but Bedrock, pinned as
    // `has_adapter` in every build (no CI row compiles `embed-bedrock`, so
    // `is_ready` alone would compare false with false for it).
    let implemented = lean.table("EmbedderKind.implemented", "EmbedderKind → Bool");
    let (ctor, listed) = lean_ctors!(EmbedderKind {
        BgeM3 => "bgeM3",
        Candle => "candle",
        Gemini => "gemini",
        Bedrock => "bedrock",
        Fixture => "fixture",
        EmbeddingGemma2 => "eg2",
        Elastic => "elastic",
    });
    implemented.check_names(&listed.map(ctor));
    for kind in listed {
        assert_pin!(kind.has_adapter(), implemented.at(ctor(kind)).bool());
        assert_eq!(
            kind.is_ready(),
            kind.is_compiled() && kind.has_adapter(),
            "{kind:?}"
        );
    }

    let pg_max_width = lean.def("pgMaxWidth", "Nat").nat().to::<usize>();
    #[cfg(feature = "store-postgres")]
    {
        use crate::store::pg::postgres::formal_pins::{
            DEFAULT_POSTGRES_VECTOR_DIM, PGVECTOR_HNSW_VECTOR_MAX_DIM,
        };

        assert_pin!(DEFAULT_POSTGRES_VECTOR_DIM, default_dim);
        assert_pin!(PGVECTOR_HNSW_VECTOR_MAX_DIM, pg_max_width);
    }
    #[cfg(not(feature = "store-postgres"))]
    let _ = pg_max_width;
}

#[test]
fn tiered() {
    use crate::store::MAX_VECTOR_CANDIDATE_LIMIT;

    let lean = Lean::new(model!("Tiered"));
    assert_pin!(
        MAX_VECTOR_CANDIDATE_LIMIT,
        lean.def("maxLimit", "Nat").nat().to::<usize>()
    );

    let nat = |name: &str| lean.def(name, "Nat").nat().to::<u64>();
    let (shift, u32_max, max_tok) = (nat("shift"), nat("u32Max"), nat("maxTok"));
    let overfetch = lean.def("overfetch", "Nat").nat().to::<usize>();
    // The breaker and the repair backoff, in whole seconds.
    let param = |name: &str| lean.field("Params", name, "Nat").nat();
    let threshold = param("threshold").to::<u32>();
    let (cooldown, backoff) = (param("cooldown").secs(), param("backoff").secs());

    #[cfg(feature = "recall-elastic")]
    {
        use crate::store::tiered::project::mirror_version;
        use crate::store::tiered::{
            BREAKER_COOLDOWN, BREAKER_THRESHOLD, KNN_OVERFETCH, REPAIR_BACKOFF,
        };

        // `mirror_version` shifts the token by 2^32, takes a u32 counter, and
        // refuses a token above 2^31 - 1.
        assert_pin!(u64::from(u32::MAX), u32_max);
        let (shift, max_tok) = (shift.value, max_tok);
        assert_eq!(
            mirror_version(max_tok.value, u32::MAX),
            Some(max_tok.value * shift + u64::from(u32::MAX)),
            "{}",
            max_tok.at
        );
        assert_eq!(mirror_version(2, 1), Some(2 * shift + 1), "shift");
        assert_eq!(mirror_version(max_tok.value + 1, 0), None, "{}", max_tok.at);
        assert_pin!(KNN_OVERFETCH, overfetch);
        assert_pin!(BREAKER_THRESHOLD, threshold);
        assert_pin!(BREAKER_COOLDOWN, cooldown);
        assert_pin!(REPAIR_BACKOFF, backoff);
    }
    #[cfg(not(feature = "recall-elastic"))]
    let _ = (
        shift, u32_max, max_tok, overfetch, threshold, cooldown, backoff,
    );
}
