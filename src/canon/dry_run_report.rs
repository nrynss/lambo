//! Rendering and comparing dry-run reports (#36 PR 5). Ids, type and status
//! labels and counts only: nothing here can print concept text, because no
//! report type carries any.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{Census, DryRunReport, IdRow, Invariant, PolicyReport, Split, Survey};
use crate::canon::PromotionPolicy;
use crate::types::NodeId;

fn split(s: &Split) -> String {
    let types: Vec<String> = s.by_type.iter().map(|(t, n)| format!("{t} {n}")).collect();
    format!(
        "{} ({} Resource; {})",
        s.total,
        s.resource,
        if types.is_empty() {
            "none".into()
        } else {
            types.join(", ")
        }
    )
}

fn census(c: &Census) -> String {
    if c.is_empty() {
        return "empty".into();
    }
    c.iter()
        .map(|(status, types)| {
            let n: usize = types.values().sum();
            let t: Vec<String> = types.iter().map(|(t, k)| format!("{t} {k}")).collect();
            format!("{status} {n} ({})", t.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn row(r: &IdRow) -> String {
    let mut s = format!(
        "{} {:?} {:?} dependents={}",
        r.id, r.concept_type, r.status, r.dependents
    );
    if let Some(d) = r.distinct {
        let _ = write!(s, " distinct={d}");
    }
    if let Some(c) = r.coverage {
        let _ = write!(s, " coverage={c:.3}");
    }
    if !r.unmet.is_empty() {
        let _ = write!(s, " unmet={}", r.unmet.join(","));
    }
    s
}

fn survey(out: &mut String, title: &str, s: &Survey) {
    let _ = writeln!(out, "  {title}:");
    if s.eligible.is_empty() {
        let _ = writeln!(out, "    eligible: none");
    }
    for (hop, n) in &s.eligible {
        let _ = writeln!(out, "    eligible {hop}: {}", split(n));
    }
    for (gate, n) in &s.unmet {
        let _ = writeln!(out, "    unmet {gate}: {n}");
    }
    for (gate, n) in &s.one_gate_away {
        let _ = writeln!(out, "    one gate away {gate}: {}", split(n));
    }
    for r in &s.one_gate_away_top {
        let _ = writeln!(out, "      {}", row(r));
    }
    let _ = writeln!(
        out,
        "    more than five dependents, not Canonical: {}",
        s.load_bearing_total
    );
    for r in &s.load_bearing {
        let _ = writeln!(out, "      {}", row(r));
    }
    for r in &s.watched {
        let _ = writeln!(out, "    watched {}", row(r));
    }
}

fn invariants(out: &mut String, indent: &str, list: &[Invariant]) {
    for i in list {
        let _ = writeln!(
            out,
            "{indent}{} {}{}",
            if i.held { "ok  " } else { "FAIL" },
            i.name,
            if i.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", i.detail)
            }
        );
    }
}

/// One policy as text.
pub(crate) fn render_policy(out: &mut String, p: &PolicyReport) {
    let _ = writeln!(out, "\n== {} ==", p.policy);
    let _ = writeln!(out, "  start: {}", census(&p.start));
    survey(out, "first evaluation (unbatched)", &p.first_evaluation);
    let _ = writeln!(
        out,
        "  run: {} cycles, stop {:?} after {} quiet cycles, budget saturated {}",
        p.cycles, p.stop, p.quiet_needed, p.budget_saturated
    );
    let _ = writeln!(out, "  first cycle hops: {:?}", p.first_cycle);
    let _ = writeln!(out, "  all hops: {:?}", p.hops);
    for status in ["Candidate", "Venerable", "Canonical"] {
        let n = p.reached.get(status).cloned().unwrap_or_default();
        let _ = writeln!(out, "  new {status}: {}", split(&n));
    }
    let _ = writeln!(out, "  demoted: {}", split(&p.demoted));
    let _ = writeln!(out, "  end: {}", census(&p.end));
    survey(out, "fixed point", &p.fixed_point);
    let _ = writeln!(out, "  canonical at the stop: {}", p.canonical_total);
    for r in &p.canonical {
        let _ = writeln!(out, "    {}", row(r));
    }
    let _ = writeln!(out, "  invariants:");
    invariants(out, "    ", &p.invariants);
}

/// A dry-run report as text.
pub fn render_report(r: &DryRunReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "canon dry-run: session={} at={} concepts={} interactions={} edges={} survival_raised={}",
        r.session,
        r.at.to_rfc3339(),
        r.concepts,
        r.interactions,
        r.edges,
        r.survival_raised
    );
    if r.supersede {
        // #20: only when on, so a default report is unchanged.
        let _ = writeln!(out, "  supersede: on (supersession, then the Stage 3 hold)");
    }
    if let Some(s) = &r.source {
        let _ = writeln!(
            out,
            "source: {} sha256 {} -> {} lease_present={} lease_token={} lease_live_at_clock={} unconsumed_intents={} (not replayed)",
            s.path,
            s.sha256_before,
            s.sha256_after,
            s.lease_present,
            s.lease_token.map_or("-".into(), |t| t.to_string()),
            s.lease_live_at_clock.map_or("-".into(), |b| b.to_string()),
            s.unconsumed_intents
        );
    }
    for p in &r.policies {
        render_policy(&mut out, p);
    }
    if !r.invariants.is_empty() {
        let _ = writeln!(out, "\nrun invariants:");
        invariants(&mut out, "  ", &r.invariants);
    }
    if let Some(c) = &r.comparison {
        out.push_str(&render_comparison(c));
    }
    out
}

/// What changed for one policy between two reports.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyDelta {
    /// The policy.
    pub policy: Option<PromotionPolicy>,
    /// `new <status>` totals, before -> after.
    pub reached: BTreeMap<String, (usize, usize)>,
    /// End status totals, before -> after.
    pub end: BTreeMap<String, (usize, usize)>,
    /// Canonicals listed after and not before.
    pub canonical_added: Vec<NodeId>,
    /// Canonicals listed before and not after.
    pub canonical_removed: Vec<NodeId>,
    /// Invariants that held before and fail after.
    pub invariants_broken: Vec<String>,
}

/// A comparison of two reports, per policy present in both.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// One per policy.
    pub policies: Vec<PolicyDelta>,
}

fn totals(c: &Census) -> BTreeMap<String, usize> {
    c.iter()
        .map(|(s, t)| (s.clone(), t.values().sum()))
        .collect()
}

/// Compare `after` with `baseline` (an earlier report, for example the
/// previous re-pin's). Canonical id sets compare every Canonical
/// (`canonical_ids`). The caller checks that the two are of one session and
/// one report version ([`baseline_mismatch`]).
pub fn compare(baseline: &DryRunReport, after: &DryRunReport) -> Comparison {
    let mut out = Comparison::default();
    for a in &after.policies {
        let Some(b) = baseline.policies.iter().find(|b| b.policy == a.policy) else {
            continue;
        };
        out.policies.push(policy_delta(b, a));
    }
    out
}

/// Why `baseline` cannot be compared with `after`, if it cannot: another
/// session, or another report version (review L2).
pub fn baseline_mismatch(baseline: &DryRunReport, after: &DryRunReport) -> Option<String> {
    if baseline.version != after.version {
        return Some(format!(
            "the baseline is report version {}, this run is {}; re-run the baseline with this build",
            baseline.version, after.version
        ));
    }
    if baseline.session != after.session {
        return Some(format!(
            "the baseline is of session {:?}, this run is of {:?}",
            baseline.session.as_str(),
            after.session.as_str()
        ));
    }
    None
}

pub(crate) fn policy_delta(b: &PolicyReport, a: &PolicyReport) -> PolicyDelta {
    let mut d = PolicyDelta {
        policy: Some(a.policy),
        ..PolicyDelta::default()
    };
    for s in ["Candidate", "Venerable", "Canonical"] {
        let n = |p: &PolicyReport| p.reached.get(s).map_or(0, |x| x.total);
        d.reached.insert(s.into(), (n(b), n(a)));
    }
    let (tb, ta) = (totals(&b.end), totals(&a.end));
    for s in tb.keys().chain(ta.keys()).collect::<BTreeSet<_>>() {
        d.end.insert(
            s.clone(),
            (
                tb.get(s).copied().unwrap_or(0),
                ta.get(s).copied().unwrap_or(0),
            ),
        );
    }
    let ids = |p: &PolicyReport| {
        p.canonical_ids
            .iter()
            .map(|id| id.0)
            .collect::<BTreeSet<_>>()
    };
    let (ib, ia) = (ids(b), ids(a));
    d.canonical_added = ia.difference(&ib).map(|u| NodeId(*u)).collect();
    d.canonical_removed = ib.difference(&ia).map(|u| NodeId(*u)).collect();
    d.invariants_broken = a
        .invariants
        .iter()
        .filter(|i| !i.held)
        .filter(|i| b.invariants.iter().any(|j| j.name == i.name && j.held))
        .map(|i| i.name.clone())
        .collect();
    d
}

/// A comparison as text.
pub fn render_comparison(c: &Comparison) -> String {
    let mut out = String::from("\ncompared with the baseline:\n");
    if c.policies.is_empty() {
        out.push_str("  no policy in common\n");
    }
    for d in &c.policies {
        let _ = writeln!(out, "  {}:", d.policy.map_or("?".into(), |p| p.to_string()));
        for (s, (b, a)) in &d.reached {
            let _ = writeln!(out, "    new {s}: {b} -> {a}");
        }
        for (s, (b, a)) in &d.end {
            let _ = writeln!(out, "    end {s}: {b} -> {a}");
        }
        for id in &d.canonical_added {
            let _ = writeln!(out, "    canonical added: {id}");
        }
        for id in &d.canonical_removed {
            let _ = writeln!(out, "    canonical removed: {id}");
        }
        for name in &d.invariants_broken {
            let _ = writeln!(out, "    invariant now failing: {name}");
        }
    }
    out
}
