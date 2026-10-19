//! The alias preview (#36 PR 5): the dry run twice, on the copy as it is and
//! on the copy with the planned alias redirects overlaid
//! ([`crate::canon::alias`]), and the concepts whose fixed-point status
//! differs. Nothing is applied: both runs are in-memory overlays of the same
//! snapshot, dropped when the preview returns.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::canon::alias::{
    self, AliasPair, AliasPlan, AliasRedirect, AliasRefusal, AliasWarning, PairSource, RefusalKind,
};
use crate::canon::dry_run::{
    self, clock_check, prepare, render_policy_for_preview, DryRunParams, Invariant, PolicyDelta,
    PolicyReport, RowsAfterClock, SourceFacts, REPORT_VERSION,
};
use crate::canon::similar::{self, SimilarParams, SimilarReport};
use crate::canon::PromotionPolicy;
use crate::graph::Graph;
use crate::types::{
    CanonizationStatus, ConceptType, EdgeType, GraphSnapshot, LamboError, NodeId, SessionId,
};

/// Where the pairs come from.
#[derive(Clone, Debug, PartialEq)]
pub enum PairInput {
    /// The leading-phrase rule ([`alias::detect`]).
    Detect,
    /// A reviewed map's text ([`alias::parse_map`]); detection is not run.
    Map(String),
    /// Near-duplicate suggestions ([`similar::detect_similar`], #178), plus
    /// the accepted pairs of an overrides file as reviewed pairs.
    Similar(Box<SimilarInput>),
}

/// [`PairInput::Similar`]'s inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarInput {
    /// Thresholds and flags. [`SimilarParams::rejected`] and
    /// [`SimilarParams::accepted`] are filled from `overrides`.
    pub params: SimilarParams,
    /// An overrides file's text ([`similar::split_overrides`]), or empty.
    pub overrides: String,
}

/// What [`pairs_for`] planned from.
struct Pairs {
    pairs: Vec<AliasPair>,
    refusals: Vec<AliasRefusal>,
    similar: Option<SimilarReport>,
}

/// The pairs `input` names on `g`.
fn pairs_for(g: &Graph, input: &PairInput) -> Result<Pairs, LamboError> {
    Ok(match input {
        PairInput::Detect => {
            let (pairs, refusals) = alias::detect(g);
            Pairs {
                pairs,
                refusals,
                similar: None,
            }
        }
        PairInput::Map(text) => {
            let (pairs, refusals) = alias::parse_map(g, text);
            Pairs {
                pairs,
                refusals,
                similar: None,
            }
        }
        PairInput::Similar(input) => {
            let (accept, reject) = similar::split_overrides(&input.overrides)
                .map_err(|e| LamboError::Config(format!("overrides: {e}")))?;
            let (accepted, mut refusals) = alias::parse_map(g, &accept);
            let (rejected, rejected_refusals) = alias::parse_map(g, &reject);
            refusals.extend(rejected_refusals);
            let mut params = input.params.clone();
            params.accepted = accepted.iter().map(|p| (p.alias, p.ruling)).collect();
            params.rejected = rejected.iter().map(|p| (p.alias, p.ruling)).collect();
            let report = similar::detect_similar(g, &params);
            let mut pairs = accepted;
            pairs.extend(report.pairs.iter().map(|p| AliasPair {
                alias: p.alias,
                ruling: p.ruling,
                source: PairSource::Similar,
                line: None,
            }));
            Pairs {
                pairs,
                refusals,
                similar: Some(report),
            }
        }
    })
}

/// A concept's role in a flip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlipRole {
    /// An accepted pair's alias.
    Alias,
    /// An accepted pair's ruling.
    Ruling,
    /// Anything else (a citer, or a concept the batch order moved).
    Other,
}

/// One concept whose fixed-point status the redirects change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flip {
    /// The node id.
    pub id: NodeId,
    /// Type label.
    pub concept_type: ConceptType,
    /// Alias, ruling or other.
    pub role: FlipRole,
    /// The status without the redirects.
    pub before: CanonizationStatus,
    /// The status with them.
    pub after: CanonizationStatus,
}

/// One policy, without and with the redirects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasPolicyPreview {
    /// The policy.
    pub policy: PromotionPolicy,
    /// The dry run on the copy as it is.
    pub before: PolicyReport,
    /// The dry run with the redirects overlaid.
    pub after: PolicyReport,
    /// Aggregate deltas.
    pub delta: PolicyDelta,
    /// Concepts whose stop status differs: every alias and ruling, then
    /// other concepts up to the report's `top`. Empty unless both runs held
    /// every invariant (review M2): a flip against a run that never reached
    /// its fixed point means nothing.
    pub flips: Vec<Flip>,
    /// How many concepts differ in all.
    pub flips_total: usize,
}

/// A whole alias preview.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasPreviewReport {
    /// [`REPORT_VERSION`].
    pub version: u32,
    /// The session.
    pub session: SessionId,
    /// The evaluation clock at the first cycle.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Concepts in the session.
    pub concepts: usize,
    /// Interactions in the session.
    pub interactions: usize,
    /// Edges in the session.
    pub edges: usize,
    /// Survival counts raised by `assume_survived`.
    pub survival_raised: usize,
    /// The source, when the CLI read one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceFacts>,
    /// Detected, reviewed or similar pairs.
    pub pairs_from: PairSource,
    /// The near-duplicate suggestions and what the guards removed, for
    /// [`PairInput::Similar`] (#178).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similar: Option<SimilarReport>,
    /// Accepted pairs and what each does.
    pub redirects: Vec<AliasRedirect>,
    /// Refused pairs and map lines.
    pub refusals: Vec<AliasRefusal>,
    /// Refusals by kind.
    pub refusal_counts: BTreeMap<String, usize>,
    /// Accepted reviewed pairs worth a second look (review L4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<AliasWarning>,
    /// Accepted aliases that keep a stored status with no citations left,
    /// by status (review M3): canonization never demotes for lost support.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases_keeping_status: BTreeMap<String, usize>,
    /// Rows dated after the clock (review M1).
    pub rows_after_clock: RowsAfterClock,
    /// Citation edges the overlay retires.
    pub edges_retired: usize,
    /// Citation edges the overlay adds.
    pub edges_added: usize,
    /// One per policy.
    pub policies: Vec<AliasPolicyPreview>,
    /// Checks over the plan and the run (the CLI adds `source_unchanged`).
    pub invariants: Vec<Invariant>,
}

/// Plan the pairs from `input` against `snap`.
pub fn plan_for(snap: &GraphSnapshot, input: &PairInput) -> Result<AliasPlan, LamboError> {
    Ok(plan_with(snap, input)?.0)
}

/// [`plan_for`], with the pairs it planned and the similarity report.
fn plan_with(
    snap: &GraphSnapshot,
    input: &PairInput,
) -> Result<(AliasPlan, Vec<AliasPair>, Option<SimilarReport>), LamboError> {
    let g = dry_run::graph_of(snap)?;
    let Pairs {
        pairs,
        refusals,
        similar,
    } = pairs_for(&g, input)?;
    Ok((alias::plan(&g, &pairs, refusals), pairs, similar))
}

/// The plan's own checks: it moves only alias citations to their ruling,
/// and planning again on the result changes nothing.
fn plan_invariants(
    snap: &GraphSnapshot,
    after: &GraphSnapshot,
    plan: &AliasPlan,
    input: &PairInput,
    planned: &[AliasPair],
) -> Result<Vec<Invariant>, LamboError> {
    let pairs: HashSet<(NodeId, NodeId)> =
        plan.redirects.iter().map(|r| (r.alias, r.ruling)).collect();
    let aliases: HashSet<NodeId> = pairs.iter().map(|p| p.0).collect();
    let rulings: HashSet<NodeId> = pairs.iter().map(|p| p.1).collect();
    let retired: HashSet<NodeId> = plan.retire.iter().copied().collect();
    let bad_retire = snap
        .edges
        .iter()
        .filter(|e| retired.contains(&e.id))
        .filter(|e| e.edge_type != EdgeType::Dependency || !aliases.contains(&e.target))
        .count();
    let bad_add = plan
        .add
        .iter()
        .filter(|e| e.edge_type != EdgeType::Dependency || !rulings.contains(&e.target))
        .count();
    let unchanged_nodes =
        snap.concepts == after.concepts && snap.interactions == after.interactions;

    // A second plan of the same pairs on the result (for a map, the same
    // text; for detection, detection again; for similarity, the same pairs:
    // moving citations can change which pairs a fresh detection suggests,
    // and the plan, not the detector, is what has to be idempotent).
    let g = dry_run::graph_of(after)?;
    let again = match input {
        PairInput::Similar(_) => alias::plan(&g, planned, Vec::new()),
        _ => {
            let Pairs {
                pairs, refusals, ..
            } = pairs_for(&g, input)?;
            alias::plan(&g, &pairs, refusals)
        }
    };
    Ok(vec![
        Invariant::new(
            "only_alias_citations_move",
            bad_retire == 0 && bad_add == 0 && unchanged_nodes,
            format!(
                "{} retired, {} added, {bad_retire} retired outside an alias citation, {bad_add} added outside a ruling citation",
                plan.retire.len(),
                plan.add.len()
            ),
        ),
        Invariant::new(
            "plan_is_idempotent",
            again.is_noop(),
            format!(
                "a second plan retires {} and adds {}",
                again.retire.len(),
                again.add.len()
            ),
        ),
    ])
}

/// Preview the alias redirects `input` names on `snap`, under every policy
/// in `policies`.
pub async fn alias_preview(
    mut snap: GraphSnapshot,
    input: &PairInput,
    policies: &[PromotionPolicy],
    base: &DryRunParams,
) -> Result<AliasPreviewReport, LamboError> {
    // #178: `prepare` clears every embedding (dry-run gates do not read
    // them). The similar detector's vector source needs them, so detect
    // before prepare when the input asks for it; Detect/Map are unchanged.
    let precomputed = if matches!(input, PairInput::Similar(_)) {
        Some(plan_with(&snap, input)?)
    } else {
        None
    };
    let raised = prepare(&mut snap, base.assume_survived);
    let (clock, rows_after_clock) = clock_check(&snap, base.now);
    let (plan, planned, similar) = match precomputed {
        Some(v) => v,
        None => plan_with(&snap, input)?,
    };
    let overlaid = alias::apply(&snap, &plan);
    let mut invariants = plan_invariants(&snap, &overlaid, &plan, input, &planned)?;

    let aliases: HashSet<NodeId> = plan.redirects.iter().map(|r| r.alias).collect();
    let rulings: HashSet<NodeId> = plan.redirects.iter().map(|r| r.ruling).collect();
    let types: HashMap<NodeId, ConceptType> = snap
        .concepts
        .iter()
        .map(|c| (c.id, c.concept_type))
        .collect();
    let mut previews = Vec::new();
    for &policy in policies {
        let mut params = base.clone();
        params.eval.promotion_policy = policy;
        let (before, sb) = dry_run::run(&snap, &params).await?;
        let (after, sa) = dry_run::run(&overlaid, &params).await?;
        let both_held = before
            .invariants
            .iter()
            .chain(after.invariants.iter())
            .all(|i| i.held);
        let mut flips: Vec<Flip> = sb
            .iter()
            .filter_map(|(id, b)| {
                let a = sa.get(id).copied().unwrap_or_default();
                (a != *b).then(|| Flip {
                    id: *id,
                    concept_type: types.get(id).copied().unwrap_or(ConceptType::Entity),
                    role: if aliases.contains(id) {
                        FlipRole::Alias
                    } else if rulings.contains(id) {
                        FlipRole::Ruling
                    } else {
                        FlipRole::Other
                    },
                    before: *b,
                    after: a,
                })
            })
            .collect();
        if !both_held {
            flips.clear();
        }
        flips.sort_by_key(|f| (f.role, f.id.0));
        let flips_total = flips.len();
        let mut others = 0;
        flips.retain(|f| {
            f.role != FlipRole::Other || {
                others += 1;
                others <= params.top
            }
        });
        let delta = dry_run::report_delta(&before, &after);
        previews.push(AliasPolicyPreview {
            policy,
            before,
            after,
            delta,
            flips,
            flips_total,
        });
    }
    let mut refusal_counts = BTreeMap::new();
    for r in &plan.refusals {
        *refusal_counts.entry(format!("{:?}", r.kind)).or_default() += 1;
    }
    // Review M2: both runs, not only the overlaid one.
    let failing: Vec<String> = previews
        .iter()
        .flat_map(|p| {
            [("without", &p.before), ("with", &p.after)]
                .into_iter()
                .flat_map(move |(w, r)| {
                    r.invariants
                        .iter()
                        .filter(|i| !i.held)
                        .map(move |i| format!("{} {w} redirects: {}", p.policy, i.name))
                })
        })
        .collect();
    invariants.push(Invariant::new(
        "both_runs_hold_their_invariants",
        failing.is_empty(),
        failing.join("; "),
    ));
    // Review H2: a reviewed map is the owner's decision, so a line it
    // cannot honour fails the preview; detected refusals are information.
    // The same for an overrides file: its lines are decisions too. A
    // refused suggested pair is not (they come from the detector).
    if matches!(input, PairInput::Map(_) | PairInput::Similar(_)) {
        let refused: Vec<&AliasRefusal> = plan
            .refusals
            .iter()
            .filter(|r| r.source == PairSource::Reviewed)
            .collect();
        invariants.push(Invariant::new(
            "every_reviewed_line_accepted",
            refused.is_empty(),
            format!("{} refused: {:?}", refused.len(), refusal_counts),
        ));
    }
    invariants.push(clock);
    let mut aliases_keeping_status = BTreeMap::new();
    for r in plan
        .redirects
        .iter()
        .filter(|r| r.alias_status != CanonizationStatus::None && r.citations_left() == 0)
    {
        *aliases_keeping_status
            .entry(format!("{:?}", r.alias_status))
            .or_default() += 1;
    }
    Ok(AliasPreviewReport {
        version: REPORT_VERSION,
        session: snap.session_id.clone(),
        at: base.now,
        concepts: snap.concepts.len(),
        interactions: snap.interactions.len(),
        edges: snap.edges.len(),
        survival_raised: raised,
        source: None,
        pairs_from: match input {
            PairInput::Detect => PairSource::Detected,
            PairInput::Map(_) => PairSource::Reviewed,
            PairInput::Similar(_) => PairSource::Similar,
        },
        similar,
        redirects: plan.redirects,
        refusals: plan.refusals,
        refusal_counts,
        warnings: plan.warnings,
        aliases_keeping_status,
        rows_after_clock,
        edges_retired: plan.retire.len(),
        edges_added: plan.add.len(),
        policies: previews,
        invariants,
    })
}

fn kind_label(k: RefusalKind) -> String {
    format!("{k:?}")
}

/// The `--similar` suggestions: one line of scores and direction, the two
/// texts, and the overrides line that records the decision.
fn render_similar(out: &mut String, sim: &SimilarReport) {
    let g = &sim.guarded;
    let _ = writeln!(
        out,
        "similar: {} suggested from {} lexical and {} vector candidates (suggestions only); guarded: too_short={} below_jaccard={} digits={} identifiers={} type={} contradicts={} canonical={} rejected={} accepted={} overlap={} capped={}",
        sim.pairs.len(),
        sim.lexical_candidates,
        sim.vector_candidates,
        g.too_short,
        g.below_jaccard,
        g.digits_differ,
        g.identifiers_differ,
        g.type_differs,
        g.contradicts,
        g.canonical,
        g.rejected,
        g.accepted,
        g.overlap,
        g.capped
    );
    let clip = |t: &str| {
        let t = t.replace(['\n', '\r'], " ");
        match t.char_indices().nth(100) {
            Some((i, _)) => format!("{}…", &t[..i]),
            None => t,
        }
    };
    for p in &sim.pairs {
        let _ = writeln!(
            out,
            "  {:?} jaccard={:.3} cosine={} alias {} (cited {}) -> ruling {} (cited {})",
            p.source,
            p.jaccard,
            p.cosine
                .map_or_else(|| "-".to_string(), |c| format!("{c:.3}")),
            p.alias,
            p.alias_citations,
            p.ruling,
            p.ruling_citations
        );
        let _ = writeln!(out, "    alias:  {}", clip(&p.alias_content));
        let _ = writeln!(out, "    ruling: {}", clip(&p.ruling_content));
        let _ = writeln!(
            out,
            "    record: accept {} {}  |  reject {} {}",
            p.alias, p.ruling, p.alias, p.ruling
        );
    }
}

/// An alias preview as text.
pub fn render_alias_preview(r: &AliasPreviewReport, verbose: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "canon alias-preview: session={} at={} concepts={} interactions={} edges={} pairs_from={:?} (preview only: nothing is applied)",
        r.session,
        r.at.to_rfc3339(),
        r.concepts,
        r.interactions,
        r.edges,
        r.pairs_from
    );
    if let Some(s) = &r.source {
        let _ = writeln!(
            out,
            "source: {} sha256 {} -> {} lease_present={}",
            s.path, s.sha256_before, s.sha256_after, s.lease_present
        );
    }
    if let Some(sim) = &r.similar {
        render_similar(&mut out, sim);
    }
    let _ = writeln!(
        out,
        "redirects accepted: {} (citations moved {}, already cited {}, self dropped {}, kept for a cycle {}); edges retired {}, added {}",
        r.redirects.len(),
        r.redirects.iter().map(|x| x.redirected).sum::<usize>(),
        r.redirects.iter().map(|x| x.already_cited).sum::<usize>(),
        r.redirects.iter().map(|x| x.self_dropped).sum::<usize>(),
        r.redirects.iter().map(|x| x.cycle_refused).sum::<usize>(),
        r.edges_retired,
        r.edges_added
    );
    for x in &r.redirects {
        let _ = writeln!(
            out,
            "  {} {:?} {:?} -> {} {:?} moved={} already={} self={} cycle={} other_dependents_kept={} ruling_fan_in={}",
            x.alias,
            x.alias_type,
            x.alias_status,
            x.ruling,
            x.ruling_type,
            x.redirected,
            x.already_cited,
            x.self_dropped,
            x.cycle_refused,
            x.other_dependents_kept,
            x.ruling_fan_in
        );
    }
    for (status, n) in &r.aliases_keeping_status {
        let _ = writeln!(
            out,
            "aliases keeping status {status} with no citations left: {n} (canonization never demotes for lost support)"
        );
    }
    for w in &r.warnings {
        let _ = writeln!(
            out,
            "warning{}: {:?} ({} -> {})",
            w.line.map_or(String::new(), |l| format!(" line {l}")),
            w.kind,
            w.alias,
            w.ruling
        );
    }
    let _ = writeln!(out, "refused: {} {:?}", r.refusals.len(), r.refusal_counts);
    for x in &r.refusals {
        let _ = writeln!(
            out,
            "  {}{} alias={} rulings=[{}]",
            kind_label(x.kind),
            x.line.map_or(String::new(), |l| format!(" line {l}")),
            x.alias.map_or("-".into(), |a| a.to_string()),
            x.rulings
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    for p in &r.policies {
        let _ = writeln!(out, "\n== {} ==", p.policy);
        for (s, (b, a)) in &p.delta.reached {
            let _ = writeln!(out, "  new {s}: {b} -> {a}");
        }
        for (s, (b, a)) in &p.delta.end {
            let _ = writeln!(out, "  end {s}: {b} -> {a}");
        }
        for (when, rows) in [
            ("without redirects", &p.before.fixed_point.watched),
            ("with redirects", &p.after.fixed_point.watched),
        ] {
            for r in rows {
                let _ = writeln!(
                    out,
                    "  watched {when}: {} {:?} {:?} dependents={} unmet={}",
                    r.id,
                    r.concept_type,
                    r.status,
                    r.dependents,
                    r.unmet.join(",")
                );
            }
        }
        let _ = writeln!(out, "  flips: {}", p.flips_total);
        for f in &p.flips {
            let _ = writeln!(
                out,
                "    {:?} {} {:?}: {:?} -> {:?}",
                f.role, f.id, f.concept_type, f.before, f.after
            );
        }
        if verbose {
            render_policy_for_preview(&mut out, "without redirects", &p.before);
            render_policy_for_preview(&mut out, "with redirects", &p.after);
        }
    }
    let _ = writeln!(out, "\npreview invariants:");
    for i in &r.invariants {
        let _ = writeln!(
            out,
            "  {} {}{}",
            if i.held { "ok  " } else { "FAIL" },
            i.name,
            if i.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", i.detail)
            }
        );
    }
    out
}
