# Wave-3 graph write-gate decisions (adve-review GRAPH-1/2/4/5, COH-3, CON-6)

- **CON-6 (D6):** blast radius stays `u64` on the `GraphStore::blast_radius`
  trait surface; `Concept.blast_radius` / `CanonizationEvent.blast_radius`
  stay `Option<i32>` (frozen types). Narrowing happens at the write gate with a
  typed error — `u32::try_from(...)` mapped to an invariant violation — never a
  silent `as` cast. Documented on the trait method.
- **COH-3 (D3):** `CanonizationEvent` gained `last_demotion_time:
  Option<DateTime<Utc>>` (serde-defaulted, skipped when None — fixtures stay
  byte-identical). `apply_canonization_transition` propagates it onto the
  concept when `Some` (demotion events always carry it; non-demotion events
  leave the field untouched — adapters use `COALESCE`). `demote()` stamps
  demoted observations with it. The matching `canonization_events` audit row
  for a write-path `demote()` is **P6's carry** — demote emits no
  `CanonizationEvent` (no `Mutation` kind exists for one), so P6's demotion
  sweep should emit the event (with the field) through the existing
  `CanonizationTransition` mutation.
- **GRAPH-4:** legal spec §10 transitions are `None→Candidate`,
  `None→Venerable`, `Candidate→Venerable`, `Venerable→Canonical`,
  `Canonical→None`; `from_status` must equal the concept's current status.
  Enforced only at `Graph::apply_canonization_transition` (the store tier
  replays validated events idempotently).
  This matrix is this review's reading of an ambiguous §10 — the spec text
  does not enumerate transitions (it lists three stage triggers plus
  demotion; allowing `None→Venerable` and rejecting `Candidate→Canonical`
  are the implementer's inferences), so P6 must treat it as a conservative
  default, not a pinned spec contract.
