//! `[serve] layers`, `[serve.session_layers]` and `[[serve.layer]]`: the
//! shared sessions a pinned session's recall also reads (#33 PR 3, design
//! sections 1 and 1.1).
//!
//! ```toml
//! [serve]
//! sessions = ["lambo", "rustydocs", "general"]
//! layers = ["general"]          # ordered stack, nearest tier first
//!
//! [serve.session_layers]        # per-session override, same ordered form
//! rustydocs = []                # opt a session out
//!
//! [[serve.layer]]               # per-layer ranking knobs
//! session = "general"
//! weight = 0.5                  # uncalibrated until #87
//! min_similarity = 0.40         # uncalibrated until #87
//! ```
//!
//! Every shape is a list, so N tiers later is one relaxed check
//! ([`MAX_LAYERS`]), not a new key. Two stages, both hard errors:
//!
//! * [`check_static`], inside `ServeConfig::validate` (every command that
//!   reads the file): names, duplicates, the stack limit, no self-layer, no
//!   transitive layering, and knobs in range that name a configured layer.
//! * [`ServeConfig::resolve_layers`], at an HTTP serve's start, against the
//!   sessions it pins (`--session` beside `[serve] sessions`): every layer
//!   and every layered session must be pinned. Credential coverage and the
//!   embedding contract are the serve's own start checks (`mcp::serve`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{addressed, serve_err, ServeConfig};
use crate::recall::layered::{DEFAULT_LAYER_MIN_SIMILARITY, DEFAULT_LAYER_WEIGHT};
use crate::types::LamboError;

/// Most layers one session may list (v1, design 1.1). Crate-private on
/// purpose: raising it is the whole change N tiers need, and it is not a
/// library API.
pub(crate) const MAX_LAYERS: usize = 1;

/// One `[[serve.layer]]` entry: how a layer's hits rank against the
/// consuming session's. Absent knobs take the library defaults
/// ([`DEFAULT_LAYER_WEIGHT`], [`DEFAULT_LAYER_MIN_SIMILARITY`]), both
/// uncalibrated until #87.
///
/// `#[non_exhaustive]`: team tiers (#131) may add knobs. Build one with
/// `LayerConfig::default()` and set its fields, or parse it from
/// `lambo.toml`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LayerConfig {
    /// The layer's session: one named in `layers` or `[serve.session_layers]`.
    pub session: String,
    /// `w` in the merge's `w / rank` key (the consuming session has 1.0).
    /// Finite and positive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// The layer's floor: a hit without keyword evidence merges only at
    /// vector cosine at or above this. In `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_similarity: Option<f64>,
}

/// One resolved layer of a session's stack.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct LayerSpec {
    /// The layer's session.
    pub session: String,
    /// Its merge weight.
    pub weight: f64,
    /// Its floor.
    pub min_similarity: f64,
}

/// The layer stack of every layered session a serve pins, resolved by
/// [`ServeConfig::resolve_layers`]. Empty for a serve with no layers, and
/// for every stdio serve (a stdio holder serves one session, unlayered).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServeLayers {
    stacks: BTreeMap<String, Vec<LayerSpec>>,
}

impl ServeLayers {
    /// No session is layered.
    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }

    /// `session`'s stack, nearest tier first; empty when it is unlayered.
    pub fn stack(&self, session: &str) -> &[LayerSpec] {
        self.stacks.get(session).map_or(&[], Vec::as_slice)
    }

    /// Whether `session` is a layer of some layered session (#33 PR 4): a
    /// shared session every project reads, whose writes refuse relative
    /// file paths.
    pub fn is_layer(&self, session: &str) -> bool {
        self.stacks
            .values()
            .any(|stack| stack.iter().any(|l| l.session == session))
    }

    /// Every layered session and its stack, by session name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[LayerSpec])> {
        self.stacks.iter().map(|(s, l)| (s.as_str(), l.as_slice()))
    }
}

/// The structural checks, run by `ServeConfig::validate`.
pub(super) fn check_static(cfg: &ServeConfig) -> Result<(), LamboError> {
    check_stack("layers", None, &cfg.layers)?;
    let mut layer_set: BTreeSet<&str> = cfg.layers.iter().map(String::as_str).collect();
    for (session, stack) in &cfg.session_layers {
        addressed("[serve.session_layers] key", session)?;
        check_stack("[serve.session_layers]", Some(session), stack)?;
        layer_set.extend(stack.iter().map(String::as_str));
    }
    // No transitive layering: a layer has no layers of its own, so each
    // consumer's list is its whole stack. `layers` never applies to a
    // layer; only an explicit non-empty override could give it one.
    for (session, stack) in &cfg.session_layers {
        if !stack.is_empty() && layer_set.contains(session.as_str()) {
            return Err(serve_err(format!(
                "[serve.session_layers] gives {session:?} layers, but {session:?} is itself a \
                 layer: a layer has no layers of its own (no transitive layering)"
            )));
        }
    }
    let mut knobs = BTreeSet::new();
    for knob in &cfg.layer {
        addressed("[[serve.layer]] session", &knob.session)?;
        if !knobs.insert(knob.session.as_str()) {
            return Err(serve_err(format!(
                "[[serve.layer]] lists session {:?} twice",
                knob.session
            )));
        }
        if !layer_set.contains(knob.session.as_str()) {
            return Err(serve_err(format!(
                "[[serve.layer]] configures {:?}, which no layers list names: list it in \
                 [serve] layers or [serve.session_layers]",
                knob.session
            )));
        }
        if let Some(w) = knob.weight
            && !(w.is_finite() && w > 0.0)
        {
            return Err(serve_err(format!(
                "[[serve.layer]] {:?}: weight must be finite and positive, got {w}",
                knob.session
            )));
        }
        if let Some(m) = knob.min_similarity
            && !(m.is_finite() && (0.0..=1.0).contains(&m))
        {
            return Err(serve_err(format!(
                "[[serve.layer]] {:?}: min_similarity must be in [0, 1], got {m}",
                knob.session
            )));
        }
    }
    Ok(())
}

/// One ordered stack: addressable names, no duplicate, not the owner
/// itself, and at most [`MAX_LAYERS`] long.
fn check_stack(field: &str, owner: Option<&str>, stack: &[String]) -> Result<(), LamboError> {
    let mut seen = BTreeSet::new();
    for layer in stack {
        addressed(&format!("{field} entry"), layer)?;
        if !seen.insert(layer.as_str()) {
            return Err(serve_err(format!("{field} lists {layer:?} twice")));
        }
        if owner == Some(layer.as_str()) {
            return Err(serve_err(format!(
                "[serve.session_layers] {layer:?} lists itself: a session cannot be its own layer"
            )));
        }
    }
    // The v1 limit (design 1.1): every shape is a list, and this is the one
    // line to relax for N tiers.
    if stack.len() > MAX_LAYERS {
        let whose = owner.map_or_else(|| "[serve] layers".to_string(), |s| format!("{s:?}"));
        return Err(serve_err(format!(
            "{whose} lists {} layers; at most {MAX_LAYERS} is supported",
            stack.len()
        )));
    }
    Ok(())
}

impl ServeConfig {
    /// Every session named as a layer anywhere: in `layers` or in a
    /// `[serve.session_layers]` list.
    fn layer_names(&self) -> BTreeSet<&str> {
        self.layers
            .iter()
            .chain(self.session_layers.values().flatten())
            .map(String::as_str)
            .collect()
    }

    /// The layers `session` lists by configuration alone, nearest tier
    /// first: its `[serve.session_layers]` override, else `layers`; empty
    /// for a session that is itself a layer. What a stdio serve checks to
    /// warn that it serves the session unlayered.
    pub fn layer_stack_of(&self, session: &str) -> &[String] {
        if self.layer_names().contains(session) {
            return &[];
        }
        self.session_layers
            .get(session)
            .unwrap_or(&self.layers)
            .as_slice()
    }

    /// Resolve the layer stacks of the sessions an HTTP serve pins
    /// (`pinned`: `--session` and `[serve] sessions` together), after
    /// [`ServeConfig::validate`].
    ///
    /// Refuses (a hard error, exit 2): a layer some pinned session would
    /// read that is not itself pinned, and a `[serve.session_layers]` key
    /// with a non-empty list that is not pinned. Pinned sessions hold their lease for the serve's
    /// life and are never idle-detached or evicted, so a recall never
    /// attaches a layer on demand and a layer never vanishes by eviction;
    /// on-demand sessions get no layers in v1.
    pub fn resolve_layers(&self, pinned: &[String]) -> Result<ServeLayers, LamboError> {
        self.validate()?;
        let is_pinned = |s: &str| pinned.iter().any(|p| p == s);
        // An empty override layers nothing, so it may name any session.
        for (session, stack) in &self.session_layers {
            if !stack.is_empty() && !is_pinned(session) {
                return Err(serve_err(format!(
                    "[serve.session_layers] names {session:?}, which this serve does not pin: \
                     only pinned sessions are layered (list it in [serve] sessions)"
                )));
            }
        }
        let mut stacks = BTreeMap::new();
        for session in pinned {
            let stack = self.layer_stack_of(session);
            if stack.is_empty() {
                continue;
            }
            for layer in stack {
                if !is_pinned(layer) {
                    return Err(serve_err(format!(
                        "{session:?} is layered with {layer:?}, which this serve does not pin: \
                         every layer must be pinned (list it in [serve] sessions), or opt \
                         {session:?} out with [serve.session_layers] {session} = []"
                    )));
                }
            }
            stacks.insert(session.clone(), self.specs_for(stack));
        }
        Ok(ServeLayers { stacks })
    }

    /// The stack `session` would have over HTTP, by configuration alone
    /// (no pinning check), for a stdio serve: it serves its one session
    /// unlayered, and says so only once it is the holder (a stdio serve
    /// that proxies to an HTTP holder gets the holder's layering).
    pub fn stdio_layers(&self, session: &str) -> ServeLayers {
        let stack = self.layer_stack_of(session);
        let mut stacks = BTreeMap::new();
        if !stack.is_empty() {
            stacks.insert(session.to_string(), self.specs_for(stack));
        }
        ServeLayers { stacks }
    }

    /// `stack` resolved against `[[serve.layer]]`, absent knobs defaulted.
    fn specs_for(&self, stack: &[String]) -> Vec<LayerSpec> {
        stack
            .iter()
            .map(|layer| {
                let k = self.layer.iter().find(|k| &k.session == layer);
                LayerSpec {
                    session: layer.clone(),
                    weight: k.and_then(|k| k.weight).unwrap_or(DEFAULT_LAYER_WEIGHT),
                    min_similarity: k
                        .and_then(|k| k.min_similarity)
                        .unwrap_or(DEFAULT_LAYER_MIN_SIMILARITY),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
