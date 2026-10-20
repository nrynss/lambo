//! `recall::layered::merge` over hand-built ranked lists: N-list interleave,
//! weights, floors, forced members, dedupe precedence, the one budget,
//! degradation and determinism.

use uuid::Uuid;

use super::*;
use crate::recall::format::{default_token_count, fit_blocks, short_id};
use crate::types::ConceptType;

/// A test hit: content, phase-1 legs `(keyword, vector)`, annotations,
/// canonical key and holder vector.
#[derive(Clone)]
struct H {
    content: String,
    keyword: Option<f64>,
    vector: Option<f64>,
    annotations: Vec<(AnnotationKind, String)>,
    key: Option<String>,
    embedding: Option<Vec<f32>>,
    status: Option<CanonizationStatus>,
    /// Positions, in the same list, of the hits its `contradicts` lines name.
    contradicts: Vec<usize>,
}

/// A hit found by keyword and vector legs, with its content as canonical key.
fn kw(content: &str) -> H {
    H {
        content: content.into(),
        keyword: Some(1.0),
        vector: Some(0.8),
        annotations: Vec::new(),
        key: Some(content.into()),
        embedding: None,
        status: None,
        contradicts: Vec::new(),
    }
}

/// A hit found only by the vector leg at `cos`.
fn vec_hit(content: &str, cos: f64) -> H {
    H {
        keyword: None,
        vector: Some(cos),
        ..kw(content)
    }
}

/// A hit with no leg score (it came in through traversal expansion).
fn expanded(content: &str) -> H {
    H {
        keyword: None,
        vector: None,
        ..kw(content)
    }
}

impl H {
    fn with(mut self, kind: AnnotationKind, text: &str) -> Self {
        self.annotations.push((kind, text.into()));
        self
    }
    fn key(mut self, key: &str) -> Self {
        self.key = Some(key.into());
        self
    }
    fn embedding(mut self, v: Vec<f32>) -> Self {
        self.embedding = Some(v);
        self
    }
    fn status(mut self, s: CanonizationStatus) -> Self {
        self.status = Some(s);
        self
    }
    /// A `contradicts` line on the hit at `pos` of the same list.
    fn contradicts(mut self, pos: usize, text: &str) -> Self {
        self.contradicts.push(pos);
        self.with(AnnotationKind::Contradicts, text)
    }
}

/// A node id unique to `(tier, pos)`.
fn nid(tier: usize, pos: usize) -> NodeId {
    NodeId(Uuid::from_u64_pair(tier as u64 + 1, pos as u64 + 1))
}

/// One session's recall as the pipeline would hand it over: hits in rank
/// order, per-hit warning lines in `warnings` (plus `extra` lines), and
/// context flags set as a per-session cut would (to show the merge resets
/// them).
fn recall(tier: usize, hits: &[H], extra: &[&str]) -> TierRecall {
    let mut out_hits = Vec::new();
    let mut detailed = Vec::new();
    let mut warnings: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let mut legs = HashMap::new();
    let mut identities = Vec::new();
    for (pos, h) in hits.iter().enumerate() {
        let hit = RecallHit {
            repo_scope: None,
            node_id: nid(tier, pos),
            content: h.content.clone(),
            concept_type: Some(ConceptType::Logic),
            score: 1.0 / (pos as f64 + 1.0),
            is_canonical: false,
            blast_radius: None,
        };
        let mut d = DetailedHit::new(&hit, h.status);
        d.contradicts = h.contradicts.iter().map(|&p| nid(tier, p)).collect();
        d.included_in_context = true;
        for (kind, text) in &h.annotations {
            d.annotations.push(Annotation::new(*kind, text.clone()));
            warnings.push(text.clone());
        }
        if h.keyword.is_some() || h.vector.is_some() {
            legs.insert(
                hit.node_id,
                LegScores {
                    keyword: h.keyword,
                    recent: None,
                    vector: h.vector,
                },
            );
        }
        identities.push(HitIdentity {
            canonical_key: h.key.clone(),
            embedding: h.embedding.clone(),
        });
        out_hits.push(hit);
        detailed.push(d);
    }
    TierRecall {
        recall: DetailedRecall {
            anchor: None,
            hits: out_hits,
            context: "ignored per-session context".into(),
            truncation: None,
            warnings,
            legs,
            cold_start: false,
            detailed,
            response_annotations: Vec::new(),
        },
        identities,
    }
}

fn primary(hits: &[H]) -> TierList {
    TierList {
        session: SessionId::new("lambo"),
        weight: PRIMARY_WEIGHT,
        floor: None,
        recall: Some(recall(0, hits, &[])),
    }
}

fn layer(tier: usize, name: &str, weight: f64, hits: &[H]) -> TierList {
    TierList {
        session: SessionId::new(name),
        weight,
        floor: Some(DEFAULT_LAYER_MIN_SIMILARITY),
        recall: Some(recall(tier, hits, &[])),
    }
}

fn unavailable(name: &str) -> TierList {
    TierList {
        session: SessionId::new(name),
        weight: DEFAULT_LAYER_WEIGHT,
        floor: Some(DEFAULT_LAYER_MIN_SIMILARITY),
        recall: None,
    }
}

fn params(top_k: usize) -> MergeParams {
    MergeParams {
        top_k,
        max_tokens: 100_000,
        dedupe_threshold: 0.85,
        vector_pairs: MAX_VECTOR_DEDUPE_PAIRS,
    }
}

fn run(lists: &[TierList], p: MergeParams) -> LayeredRecall {
    merge(lists, p, default_token_count)
}

/// `session/content` per merged hit, in merged order.
fn order(r: &LayeredRecall) -> Vec<String> {
    r.hits
        .iter()
        .map(|h| format!("{}/{}", h.session, h.hit.content))
        .collect()
}

#[test]
fn two_lists_interleave_by_weight_over_rank() {
    let lists = [
        primary(&[kw("p1"), kw("p2"), kw("p3"), kw("p4")]),
        layer(1, "general", 0.5, &[kw("g1"), kw("g2"), kw("g3")]),
    ];
    let r = run(&lists, params(10));
    // keys: p1 1, p2 .5, g1 .5, p3 .33, p4 .25, g2 .25, g3 .17; ties go to
    // the nearer tier.
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p2",
            "general/g1",
            "lambo/p3",
            "lambo/p4",
            "general/g2",
            "general/g3"
        ]
    );
    // top_k bounds the merged list.
    let r = run(&lists, params(3));
    assert_eq!(order(&r), ["lambo/p1", "lambo/p2", "general/g1"]);
    assert!(r.unavailable_layers.is_empty());
}

#[test]
fn three_lists_interleave_with_per_layer_weights() {
    let lists = [
        primary(&[kw("p1"), kw("p2")]),
        layer(1, "team", 0.5, &[kw("t1"), kw("t2")]),
        layer(2, "general", 0.25, &[kw("g1"), kw("g2")]),
    ];
    // keys: p1 1, p2 .5, t1 .5, t2 .25, g1 .25, g2 .125.
    let r = run(&lists, params(10));
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p2",
            "team/t1",
            "team/t2",
            "general/g1",
            "general/g2"
        ]
    );
    let r = run(&lists, params(4));
    assert_eq!(order(&r), ["lambo/p1", "lambo/p2", "team/t1", "team/t2"]);
}

#[test]
fn weights_move_a_layer_up_or_down() {
    let heavy = [
        primary(&[kw("p1"), kw("p2")]),
        layer(1, "general", 2.0, &[kw("g1"), kw("g2")]),
    ];
    // keys: g1 2, p1 1, g2 1, p2 .5.
    assert_eq!(
        order(&run(&heavy, params(10))),
        ["general/g1", "lambo/p1", "general/g2", "lambo/p2"]
    );
    let zero = [
        primary(&[kw("p1"), kw("p2")]),
        layer(1, "general", 0.0, &[kw("g1")]),
    ];
    assert_eq!(
        order(&run(&zero, params(10))),
        ["lambo/p1", "lambo/p2", "general/g1"]
    );
    assert_eq!(order(&run(&zero, params(2))), ["lambo/p1", "lambo/p2"]);
}

#[test]
fn the_absolute_floor_rejects_keyword_matches_without_relevance() {
    let mut keyword_only = kw("g-keyword-only");
    keyword_only.vector = None;
    let mut weak_keyword = kw("g-weak-keyword");
    weak_keyword.vector = Some(DEFAULT_LAYER_MIN_SIMILARITY - 0.01);
    let lists = [
        primary(&[expanded("p-expanded"), vec_hit("p-weak", 0.1)]),
        layer(
            1,
            "general",
            DEFAULT_LAYER_WEIGHT,
            &[
                vec_hit("g-below", DEFAULT_LAYER_MIN_SIMILARITY - 0.01),
                vec_hit("g-at", DEFAULT_LAYER_MIN_SIMILARITY),
                keyword_only,
                weak_keyword.with(AnnotationKind::Hot, "weak keyword warning"),
                expanded("g-expanded"),
                vec_hit("g-above", 0.9),
            ],
        ),
    ];
    let r = run(&lists, params(10));
    assert_eq!(
        order(&r),
        [
            "lambo/p-expanded",
            "lambo/p-weak",
            "general/g-at",
            "general/g-above"
        ]
    );
    assert!(r.warnings.is_empty(), "rejected layer warnings stay out");
    let mut strict = lists.clone();
    strict[1].floor = Some(0.95);
    assert_eq!(
        order(&run(&strict, params(10))),
        ["lambo/p-expanded", "lambo/p-weak"]
    );
}

#[test]
fn tuned_defaults_admit_relevant_rules_and_reject_shared_vocabulary() {
    let mut unrelated = kw("shared vocabulary");
    unrelated.vector = Some(0.49);
    let lists = [
        primary(&[kw("p1"), kw("p2"), kw("p3"), kw("p4"), kw("p5")]),
        layer(
            1,
            "general",
            DEFAULT_LAYER_WEIGHT,
            &[
                unrelated.with(AnnotationKind::Hot, "unrelated warning"),
                vec_hit("relevant rule", 0.51),
            ],
        ),
    ];
    let r = run(&lists, params(5));
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p2",
            "lambo/p3",
            "lambo/p4",
            "general/relevant rule"
        ]
    );
    assert!(r.warnings.is_empty());
}

#[test]
fn forced_members_pass_top_k_but_never_the_floor() {
    let lists = [
        primary(&[
            kw("p1"),
            kw("p2").with(AnnotationKind::LoadBearing, "⚑ p2 pillar"),
        ]),
        layer(
            1,
            "general",
            0.5,
            &[
                kw("g1"),
                kw("g-conflict").with(AnnotationKind::Conflict, "Agent B wrote g-conflict"),
                vec_hit("g-reserved", 0.5).with(AnnotationKind::Reservation, "Reserved by x"),
                kw("g-hot").with(AnnotationKind::Hot, "High-risk modification: y"),
                // Forced kinds without evidence: a busy shared session's
                // hot, reserved or conflicting concepts at low similarity,
                // or reached by expansion only, stay out.
                vec_hit("g-weak-conflict", 0.1).with(AnnotationKind::Conflict, "Agent C wrote it"),
                expanded("g-expanded-reserved").with(AnnotationKind::Reservation, "Reserved by z"),
                expanded("g-expanded-hot").with(AnnotationKind::Hot, "Drifted 3 hops"),
                kw("g-plain"),
            ],
        ),
    ];
    let r = run(&lists, params(1));
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "general/g-conflict",
            "general/g-reserved",
            "general/g-hot"
        ]
    );
    // Forced layer hits' lines stay, prefixed; the cut p2's line and the
    // floored hits' lines leave.
    assert_eq!(
        r.warnings,
        [
            "[general] Agent B wrote g-conflict",
            "[general] Reserved by x",
            "[general] High-risk modification: y"
        ]
    );
    assert_eq!(r.detail.warnings, r.warnings);
}

#[test]
fn a_layer_traversal_answer_is_anchored_evidence_with_its_headline_recomputed() {
    // A layer that answered a dependency question by traversal: no leg
    // scores, a traversal headline counting its own cut.
    let mut g = layer(
        1,
        "general",
        2.0,
        &[
            expanded("dep one"),
            expanded("dep two"),
            expanded("dep three"),
        ],
    );
    let t = g.recall.as_mut().unwrap();
    let old = crate::recall::dispatch::traversal_headline(3);
    t.recall.warnings.insert(0, old.clone());
    t.recall
        .response_annotations
        .push(Annotation::new(AnnotationKind::Traversal, old));
    let lists = [primary(&[kw("p1")]), g];
    let r = run(&lists, params(10));
    assert_eq!(
        order(&r),
        [
            "general/dep one",
            "lambo/p1",
            "general/dep two",
            "general/dep three"
        ]
    );
    assert_eq!(
        r.warnings,
        ["[general] recall: dependency question answered by graph traversal (3 dependents)"]
    );
    // A budget for the first two blocks: the layer rendered one dependent.
    let two = fit_blocks(
        &[
            block_with_meta(
                &r.detail.hits[0],
                &[],
                &["from general".into()],
                Some("general".into()),
            ),
            block_with_meta(&r.detail.hits[1], &[], &[], None),
        ],
        100_000,
        default_token_count,
    );
    let cut = run(
        &lists,
        MergeParams {
            max_tokens: default_token_count(&two.context),
            ..params(10)
        },
    );
    let headline =
        "[general] recall: dependency question answered by graph traversal (1 dependents)";
    assert_eq!(cut.warnings, [headline]);
    assert_eq!(cut.detail.response_annotations[0].text, headline);
    assert_eq!(
        cut.detail.response_annotations[0].kind,
        AnnotationKind::Traversal
    );
    // Without the traversal annotation the same hits fail the floor.
    let mut plain = lists.clone();
    let t = plain[1].recall.as_mut().unwrap();
    t.recall.response_annotations.clear();
    assert_eq!(order(&run(&plain, params(10))), ["lambo/p1"]);
}

#[test]
fn the_callers_own_traversal_headline_counts_its_rendered_dependents() {
    let mut p = primary(&[expanded("dep a"), expanded("dep b")]);
    let t = p.recall.as_mut().unwrap();
    let old = crate::recall::dispatch::traversal_headline(2);
    t.recall.warnings.push(old.clone());
    t.recall
        .response_annotations
        .push(Annotation::new(AnnotationKind::Traversal, old));
    let lists = [p, layer(1, "general", 0.5, &[kw("g1")])];
    let one = fit_blocks(
        &[block_with_meta(
            &lists[0].recall.as_ref().unwrap().recall.hits[0],
            &[],
            &[],
            None,
        )],
        100_000,
        default_token_count,
    );
    let r = run(
        &lists,
        MergeParams {
            max_tokens: default_token_count(&one.context),
            ..params(10)
        },
    );
    assert_eq!(
        r.warnings,
        ["recall: dependency question answered by graph traversal (1 dependents)"]
    );
}

#[test]
fn dedupe_keeps_the_nearer_tier_at_the_better_rank() {
    let lists = [
        primary(&[
            kw("p1"),
            kw("p2"),
            kw("p3"),
            kw("same, primary copy").key("k"),
        ]),
        layer(
            1,
            "general",
            0.5,
            &[kw("same, general copy")
                .key("k")
                .with(AnnotationKind::LoadBearing, "⚑ general pillar")],
        ),
    ];
    let r = run(&lists, params(10));
    // The primary copy (key .25) takes the layer copy's key (.5) and sits
    // after p2 (equal key, its own position later).
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p2",
            "lambo/same, primary copy",
            "lambo/p3"
        ]
    );
    let kept = &r.hits[2];
    assert_eq!(
        kept.also,
        [LayerCopy {
            session: SessionId::new("general"),
            node_id: nid(1, 0)
        }]
    );
    let also = format!("also {} in general)", short_id(nid(1, 0)));
    assert!(r.context.contains(&also), "{}", r.context);
    // The dropped copy's warning line still reaches `warnings`.
    assert_eq!(r.warnings, ["[general] ⚑ general pillar"]);
    // A forced copy that joins an emitted group adds no slot; a forced
    // layer copy of a primary hit cut by top_k is emitted, shown as the
    // nearer (primary) copy.
    let lists = [
        primary(&[kw("p1"), kw("same").key("k")]),
        layer(
            1,
            "general",
            0.5,
            &[kw("same")
                .key("k")
                .with(AnnotationKind::Conflict, "Agent B wrote it")],
        ),
    ];
    assert_eq!(order(&run(&lists, params(1))), ["lambo/p1", "lambo/same"]);
}

#[test]
fn dedupe_precedence_follows_tier_order_over_three_lists() {
    let v = |x: f32, y: f32| vec![x, y];
    let lists = [
        primary(&[kw("p1")]),
        layer(
            1,
            "team",
            0.5,
            &[kw("team copy").key("a").embedding(v(1.0, 0.0))],
        ),
        layer(
            2,
            "general",
            0.5,
            &[
                // Cosine ~0.995 with the team copy: the same memory.
                kw("general copy").key("b").embedding(v(1.0, 0.1)),
                // Cosine 0.8 < 0.85: a different memory.
                kw("general other").key("c").embedding(v(0.8, 0.6)),
            ],
        ),
    ];
    let r = run(&lists, params(10));
    assert_eq!(
        order(&r),
        ["lambo/p1", "team/team copy", "general/general other"]
    );
    assert_eq!(r.hits[1].also[0].session, SessionId::new("general"));
    // Without vectors only the key decides.
    let mut no_vectors = lists.clone();
    for l in &mut no_vectors {
        for id in &mut l.recall.as_mut().unwrap().identities {
            id.embedding = None;
        }
    }
    assert_eq!(run(&no_vectors, params(10)).hits.len(), 4);
    // Hits of one list are never deduped against each other.
    let same_list = [
        primary(&[kw("x").key("k"), kw("y").key("k")]),
        layer(1, "general", 0.5, &[]),
    ];
    assert_eq!(run(&same_list, params(10)).hits.len(), 2);
}

#[test]
fn one_budget_over_the_merged_blocks() {
    let lists = [
        primary(&[kw("primary fact one"), kw("primary fact two")]),
        layer(1, "general", 0.5, &[kw("general ruling")]),
    ];
    let full = run(&lists, params(10));
    // Each session's own context is ignored; the merged one names the layer.
    assert!(!full.context.contains("ignored"));
    let g = short_id(nid(1, 0));
    assert!(
        full.context.contains(&format!(
            "general ruling [Logic] (score 1.00, id {g}, from general)"
        )),
        "{}",
        full.context
    );
    assert!(full.hits.iter().all(|h| h.included_in_context));
    // A budget for two whole blocks: the third is out, flags follow the cut.
    let two = fit_blocks(
        &[
            block_with_meta(&full.detail.hits[0], &[], &[], None),
            block_with_meta(&full.detail.hits[1], &[], &[], None),
        ],
        100_000,
        default_token_count,
    );
    let cut = run(
        &lists,
        MergeParams {
            max_tokens: default_token_count(&two.context),
            ..params(10)
        },
    );
    assert_eq!(cut.context, two.context);
    assert_eq!(
        cut.hits
            .iter()
            .map(|h| h.included_in_context)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
}

#[test]
fn a_truncated_layer_first_block_names_its_scope() {
    let long = "general ruling ".repeat(200);
    let lists = [
        primary(&[kw("p1")]),
        layer(1, "general", 2.0, &[kw(long.trim())]),
    ];
    let r = run(
        &lists,
        MergeParams {
            max_tokens: 120,
            ..params(10)
        },
    );
    assert_eq!(r.hits[0].session, SessionId::new("general"));
    assert!(r.hits[0].truncated_in_context);
    assert!(!r.hits[0].included_in_context);
    assert!(!r.hits[1].included_in_context);
    let marker = format!(
        "lambo_inspect {} (scope general) for the full concept]",
        short_id(nid(1, 0))
    );
    assert!(r.context.ends_with(&marker), "{}", r.context);
    assert!(r.context.contains(", from general)"), "{}", r.context);
    assert!(r.detail.truncated_hit().is_some());
    assert!(default_token_count(&r.context) <= 120);
}

#[test]
fn an_unavailable_layer_leaves_the_primary_answer_and_says_so() {
    let p = [
        kw("p1"),
        kw("p2").with(AnnotationKind::Reservation, "Reserved by x"),
    ];
    let mut alone = primary(&p);
    alone
        .recall
        .as_mut()
        .unwrap()
        .recall
        .warnings
        .insert(0, "recall: embed failed".into());
    let lists = [alone.clone(), unavailable("general")];
    let r = run(&lists, params(10));
    assert_eq!(order(&r), ["lambo/p1", "lambo/p2"]);
    assert_eq!(r.unavailable_layers, [SessionId::new("general")]);
    assert_eq!(
        r.warnings,
        [
            "recall: embed failed",
            "Reserved by x",
            "recall: layer general is unavailable; results are from lambo only"
        ]
    );
    let annotations: Vec<_> = r
        .detail
        .response_annotations
        .iter()
        .map(|a| a.kind)
        .collect();
    assert_eq!(annotations, [AnnotationKind::LayerUnavailable]);
    // The context is exactly what the caller's own blocks render to.
    let own = alone.recall.as_ref().unwrap();
    let blocks: Vec<_> = own
        .recall
        .hits
        .iter()
        .zip(&own.recall.detailed)
        .map(|(h, d)| {
            let lines: Vec<String> = d.annotations.iter().map(|a| a.text.clone()).collect();
            ContextBlock::for_hit(h, &lines)
        })
        .collect();
    assert_eq!(
        r.context,
        fit_blocks(&blocks, 100_000, default_token_count).context
    );

    // Three tiers, one down: the line names the sessions that answered.
    let lists = [
        primary(&[kw("p1")]),
        unavailable("team"),
        layer(2, "general", 0.5, &[kw("g1")]),
    ];
    let r = run(&lists, params(10));
    assert_eq!(order(&r), ["lambo/p1", "general/g1"]);
    assert_eq!(
        r.warnings,
        ["recall: layer team is unavailable; results are from lambo, general"]
    );
}

#[test]
fn layer_warnings_and_response_annotations_are_prefixed() {
    let mut g = layer(1, "general", 0.5, &[kw("g1")]);
    let t = g.recall.as_mut().unwrap();
    t.recall.warnings.push("recall: vector leg skipped".into());
    t.recall.response_annotations.push(Annotation::new(
        AnnotationKind::VectorDegraded,
        "recall: vector leg skipped",
    ));
    let r = run(&[primary(&[kw("p1")]), g], params(10));
    assert_eq!(r.warnings, ["[general] recall: vector leg skipped"]);
    assert_eq!(
        r.detail.response_annotations,
        [Annotation::new(
            AnnotationKind::VectorDegraded,
            "[general] recall: vector leg skipped"
        )]
    );
}

#[test]
fn the_merge_is_deterministic_with_total_tie_breaks() {
    let lists = [
        primary(&[kw("p1"), kw("p2")]),
        layer(1, "team", 0.5, &[kw("t1"), kw("t2")]),
        layer(2, "general", 0.5, &[kw("g1"), kw("g2")]),
    ];
    // p2, t1 and g1 all key .5; t2 and g2 key .25: tier order decides.
    let a = run(&lists, params(10));
    assert_eq!(
        order(&a),
        [
            "lambo/p1",
            "lambo/p2",
            "team/t1",
            "general/g1",
            "team/t2",
            "general/g2"
        ]
    );
    for _ in 0..5 {
        let b = run(&lists, params(10));
        assert_eq!(a.hits, b.hits);
        assert_eq!(a.context, b.context);
        assert_eq!(a.warnings, b.warnings);
    }
}

#[test]
fn block_with_meta_without_meta_is_the_plain_block() {
    let hit = RecallHit {
        repo_scope: None,
        node_id: nid(0, 0),
        content: "c".into(),
        concept_type: Some(ConceptType::Entity),
        score: 0.5,
        is_canonical: true,
        blast_radius: Some(3),
    };
    let lines = vec!["⚑ pillar".to_string()];
    assert_eq!(
        block_with_meta(&hit, &lines, &[], None),
        ContextBlock::for_hit(&hit, &lines)
    );
    let b = block_with_meta(
        &hit,
        &lines,
        &["from general".into()],
        Some("general".into()),
    );
    assert_eq!(
        &b.text[..b.label_end],
        format!(
            "c [Entity, canonical] (score 0.50, id {}, blast radius 3, from general)",
            short_id(hit.node_id)
        )
    );
    assert!(b.text[b.label_end..].starts_with("\n⚑ pillar"));
    assert_eq!(b.scope.as_deref(), Some("general"));
}

#[test]
fn dedupe_is_transitive_and_takes_one_copy_per_tier() {
    let v = |x: f32, y: f32| vec![x, y];
    let lists = [
        primary(&[kw("p1"), kw("primary copy").key("k")]),
        // Same key as the primary copy, and a vector.
        layer(
            1,
            "team",
            0.5,
            &[kw("team copy").key("k").embedding(v(1.0, 0.0))],
        ),
        // Another key, but the team copy's vector: same memory through the
        // team member of the group, not through the shown primary copy
        // (which has no vector).
        layer(
            2,
            "general",
            0.5,
            &[kw("general copy").key("other").embedding(v(1.0, 0.05))],
        ),
    ];
    let r = run(&lists, params(10));
    assert_eq!(order(&r), ["lambo/p1", "lambo/primary copy"]);
    let also: Vec<&str> = r.hits[1].also.iter().map(|a| a.session.as_str()).collect();
    assert_eq!(also, ["team", "general"]);

    // Two hits of one tier with the same key: the group takes one, the
    // other is a hit of its own.
    let lists = [
        primary(&[kw("p1").key("k")]),
        layer(1, "general", 0.5, &[kw("g1").key("k"), kw("g2").key("k")]),
    ];
    let r = run(&lists, params(10));
    assert_eq!(order(&r), ["lambo/p1", "general/g2"]);
    assert_eq!(r.hits[0].also.len(), 1);
}

#[test]
fn vector_dedupe_stops_at_its_budget_and_keys_still_match() {
    let v = vec![1.0f32, 0.0];
    let lists = [
        primary(&[kw("p1").key("a").embedding(v.clone())]),
        layer(
            1,
            "team",
            0.5,
            &[kw("same vector").key("b").embedding(v.clone())],
        ),
        layer(2, "general", 0.5, &[kw("same key").key("a")]),
    ];
    let full = run(&lists, params(10));
    assert_eq!(order(&full), ["lambo/p1"]);
    assert_eq!(full.hits[0].also.len(), 2);
    let capped = run(
        &lists,
        MergeParams {
            vector_pairs: 0,
            ..params(10)
        },
    );
    // No vector comparison is made; the key still dedupes.
    assert_eq!(order(&capped), ["lambo/p1", "team/same vector"]);
    assert_eq!(capped.hits[0].also.len(), 1);
}

#[test]
fn per_hit_legs_survive_a_node_id_shared_across_sessions() {
    let mut g = layer(1, "general", 0.5, &[vec_hit("g", 0.9).key("g-key")]);
    let t = g.recall.as_mut().unwrap();
    // The #33 split replicates identity nodes: same UUID, another session.
    let shared = nid(0, 0);
    let legs = t.recall.legs.remove(&t.recall.hits[0].node_id).unwrap();
    t.recall.hits[0].node_id = shared;
    t.recall.detailed[0].node_id = shared;
    t.recall.legs.insert(shared, legs);
    let r = run(&[primary(&[kw("p")]), g], params(10));
    assert_eq!(order(&r), ["lambo/p", "general/g"]);
    assert_eq!(r.hits[0].hit.node_id, r.hits[1].hit.node_id);
    assert_eq!(r.hit_legs[0].and_then(|l| l.keyword), Some(1.0));
    assert_eq!(r.hit_legs[1].and_then(|l| l.vector), Some(0.9));
    // The caller-keyed map holds the caller's legs only.
    assert_eq!(r.detail.legs.len(), 1);
    assert_eq!(r.detail.legs[&shared].keyword, Some(1.0));
}

#[test]
fn layer_unavailable_is_a_snake_case_kind_on_the_wire() {
    let a = Annotation::new(AnnotationKind::LayerUnavailable, "x");
    let json = serde_json::to_string(&a).unwrap();
    assert_eq!(json, r#"{"kind":"layer_unavailable","text":"x"}"#);
    assert_eq!(serde_json::from_str::<Annotation>(&json).unwrap(), a);
}

#[test]
fn a_different_layer_anchor_cannot_admit_dependents_past_the_floor() {
    let mut caller = primary(&[kw("caller dependent")]);
    caller.recall.as_mut().unwrap().recall.anchor = Some("user db schema".into());
    let mut general = layer(1, "general", 1.0, &[expanded("db dependent")]);
    let t = general.recall.as_mut().unwrap();
    t.recall.anchor = Some("db".into());
    let headline = crate::recall::dispatch::traversal_headline(1);
    t.recall.warnings.push(headline.clone());
    t.recall
        .response_annotations
        .push(Annotation::new(AnnotationKind::Traversal, headline));
    let rejected = run(&[caller.clone(), general.clone()], params(10));
    assert_eq!(order(&rejected), ["lambo/caller dependent"]);
    assert!(rejected.warnings.is_empty());
    general.recall.as_mut().unwrap().recall.anchor = Some("user db schema".into());
    let accepted = run(&[caller, general], params(10));
    assert!(order(&accepted).contains(&"general/db dependent".to_string()));
}

/// The two-list shape of the contradiction tests: in each list a hit at
/// position 1 contradicted by the companion at position 2. The layer's
/// companion has no leg evidence, so its floor holds.
fn contradicted_lists(status: Option<CanonizationStatus>) -> [TierList; 2] {
    let target = |h: H| match status {
        Some(s) => h.status(s),
        None => h,
    };
    [
        primary(&[
            kw("p1"),
            target(kw("p-contradicted").with(
                AnnotationKind::Contradiction,
                "⚠ Contradicted at t by agent-c: \"p-companion\" (id aaaaaaaa)",
            )),
            expanded("p-companion").contradicts(1, "⚠ Contradicts \"p-contradicted\""),
        ]),
        layer(
            1,
            "general",
            0.5,
            &[
                kw("g1"),
                target(kw("g-contradicted").with(
                    AnnotationKind::Contradiction,
                    "⚠ Contradicted at t by agent-d: \"g-companion\" (id bbbbbbbb)",
                )),
                expanded("g-companion").contradicts(1, "⚠ Contradicts \"g-contradicted\""),
            ],
        ),
    ]
}

/// #19(c): a contradicted Venerable hit and its companion are forced (they
/// lift `top_k`) on either side of the merge, and never past a layer's
/// floor. A companion floored out of a layer is still named, prefixed, on
/// its target's line.
#[test]
fn contradiction_annotations_force_past_top_k_but_never_the_floor() {
    let r = run(
        &contradicted_lists(Some(CanonizationStatus::Venerable)),
        params(1),
    );
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p-contradicted",
            "lambo/p-companion",
            "general/g-contradicted"
        ]
    );
    assert_eq!(
        r.hits[2].companion_of,
        Vec::<NodeId>::new(),
        "test hit has none"
    );
    assert!(r
        .warnings
        .iter()
        .any(|w| w.starts_with("[general] ⚠ Contradicted") && w.contains("bbbbbbbb")));
    assert!(!r
        .warnings
        .iter()
        .any(|w| w.contains("Contradicts \"g-contradicted\"")));
}

/// #179: an ordinary contradicted hit is not forced. Cut by `top_k`, it is
/// not shown, and neither is its companion; given a slot, it brings its
/// companion in beyond `top_k`, marked as one.
#[test]
fn an_ordinary_contradicted_hit_must_earn_its_merged_slot() {
    let cut = run(&contradicted_lists(None), params(1));
    assert_eq!(order(&cut), ["lambo/p1"]);

    let shown = run(&contradicted_lists(None), params(2));
    assert_eq!(
        order(&shown),
        ["lambo/p1", "lambo/p-contradicted", "lambo/p-companion"],
        "g1 ties p-contradicted's key and loses on tier; the companion holds no slot"
    );
    assert_eq!(shown.hits[2].companion_of, vec![nid(0, 1)]);
    assert!(shown.hits[1].companion_of.is_empty());

    // With room for everything, the companion earns a slot of its own.
    let all = run(&contradicted_lists(None), params(10));
    let p = all
        .hits
        .iter()
        .find(|h| h.hit.content == "p-companion")
        .expect("shown");
    assert!(p.companion_of.is_empty(), "it holds a slot");
    assert!(
        !order(&all).contains(&"general/g-companion".to_string()),
        "floored"
    );
}

#[test]
fn contradiction_kinds_are_snake_case_on_the_wire() {
    assert_eq!(
        serde_json::to_string(&AnnotationKind::Contradiction).unwrap(),
        "\"contradiction\""
    );
    assert_eq!(
        serde_json::to_string(&AnnotationKind::Contradicts).unwrap(),
        "\"contradicts\""
    );
}

/// #179 review: a pending companion is one memory with its copies in other
/// layers. A forced copy emitted later in the walk absorbs it, and one
/// added after the walk absorbs the cut copies, so the memory is never
/// shown twice.
#[test]
fn a_pending_companion_and_its_copies_are_shown_once() {
    let lists = |forced_copy: bool| {
        let copy = kw("g-shared").key("shared");
        let copy = if forced_copy {
            copy.with(AnnotationKind::Hot, "⚠ hot")
        } else {
            copy
        };
        [
            primary(&[
                kw("p1"),
                kw("p-contradicted").with(
                    AnnotationKind::Contradiction,
                    "⚠ Contradicted at t by agent-c: \"p-companion\" (id aaaaaaaa)",
                ),
                expanded("p-companion")
                    .key("shared")
                    .contradicts(1, "⚠ Contradicts \"p-contradicted\""),
            ]),
            layer(1, "general", 0.5, &[kw("g1"), kw("g2"), copy]),
        ]
    };
    // A forced copy later in the walk takes the companion in as its nearer
    // copy; the memory is shown once, holding no slot as a forced hit.
    let r = run(&lists(true), params(3));
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p-contradicted",
            "general/g1",
            "lambo/p-companion"
        ]
    );
    assert_eq!(r.hits[3].also.len(), 1, "the general copy");
    // Not forced: the companion is added after the walk and takes the cut
    // copy in; it stays the shown copy and still names its target.
    let r = run(&lists(false), params(3));
    assert_eq!(
        order(&r),
        [
            "lambo/p1",
            "lambo/p-contradicted",
            "general/g1",
            "lambo/p-companion"
        ]
    );
    assert_eq!(r.hits[3].also.len(), 1, "the cut general copy");
    assert_eq!(r.hits[3].companion_of, vec![nid(0, 1)]);
}
