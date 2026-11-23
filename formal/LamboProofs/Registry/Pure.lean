import LamboProofs.Registry.Step

/-
# The registry's pure pieces

Models the registry's functions that hold no state: `LeaseLossPolicy::for_scope`
and `too_many_stdio_sessions` (`src/main.rs`, `forScope`, `stdioAccepts`);
`PINNED_RETRY`'s backoff (`pinnedRetry`); `unplaced_cap`, `place_share` and
`choose_victim` in `src/mcp/serve/registry/on_demand.rs` (`unplacedCap`,
`placeShare`, `chooseVictim`) with `http_guards::credential_share`;
`answer_for` and `lookup` in `registry.rs` (`answer`); `slot_views` in
`registry/views.rs` (`slotViews`); and `NegativeCache::get` and what
`Flight::end` caches (`negGet`, `negPut`, `endOutcome`).

Results: one pinned session and nothing on demand exits the process on a
lost lease, anything more detaches the session, and a stdio serve is a
registry of one under `ExitProcess` (`forScope_exit_iff`,
`stdio_exits_on_loss`); `PINNED_RETRY` is at least a second and at least
`ELECTION_RETRY` (`pinnedRetry_ge`); the unplaced cap and every share are
positive and shares never add up past the places (`unplacedCap_pos`,
`placeShare_pos`, `placeShare_fair`); `choose_victim` takes only an eligible
session, finds none exactly when none is eligible, takes the least recently
used, and a credential at or over its share evicts only its own sessions
(`chooseVictim_eligible`, `chooseVictim_none_iff`, `chooseVictim_max`,
`at_share_evicts_own`, #32); every 503 but `Failed`'s carries a `Retry-After`
of at least a second, a session still being erased is 503, never the erased
410, and only a live slot is served (`answer_retry_after_ge`,
`answer_erasing`, `answer_live_iff`); `/admin/sessions` lists pinned sessions
first and every slot (`slotViews_pinned_first`, `slotViews_complete`); a
create request never reads a cached `absent`, a cached answer expires at its
TTL, held-elsewhere, transient, closing and timeout answers are never cached,
and a panic after admission does not block the session
(`negGet_create_not_absent`, `negGet_expired`, `busy_never_cached`,
`panic_after_admit`).

Boundary: these are the functions alone; how the step relation uses them is
`Step.lean`, which models which victim `reserve_place` takes as any one the
filter allows. `NegativeCache::put`'s size bound is not proved.
-/

namespace Lambo.Registry
/-! ## The pure pieces -/

section Pure

/-! ### Which serves detach a session on a lost lease -/

inductive Policy where
  | exitProcess
  | detachSession
  deriving DecidableEq, Repr

/-- `LeaseLossPolicy::for_scope`: one pinned session and nothing on demand
exits the process on a lost lease; anything more detaches the session. -/
def forScope (pinned : Nat) (onDemand : Bool) : Policy :=
  if pinned > 1 ∨ onDemand = true then .detachSession else .exitProcess

theorem forScope_exit_iff (p : Nat) (od : Bool) :
    forScope p od = .exitProcess ↔ p ≤ 1 ∧ od = false := by
  unfold forScope
  split
  · rename_i h
    constructor
    · intro e; cases e
    · intro ⟨h1, h2⟩; rcases h with h | h
      · omega
      · rw [h2] at h; cases h
  · rename_i h
    constructor
    · intro _
      constructor
      · omega
      · cases od
        · rfl
        · exact absurd (Or.inr rfl) h
    · intro _; rfl

/-- The stdio cap (`too_many_stdio_sessions`, exit 2): a stdio serve takes at
most one `--session`. -/
def stdioAccepts (sessions : Nat) : Bool := decide (sessions ≤ 1)

/-- A stdio serve's registry is a registry of one under `ExitProcess`: no
watcher, and `spawn_retry_loop` starts nothing (test
`one_pinned_session_exits_on_lease_loss_and_more_detach`). -/
theorem stdio_exits_on_loss {n : Nat} (h : stdioAccepts n = true) :
    forScope n false = .exitProcess := by
  unfold stdioAccepts at h
  exact (forScope_exit_iff n false).2 ⟨by simpa using h, rfl⟩

example : forScope 1 false = .exitProcess := by decide
example : forScope 2 false = .detachSession := by decide
example : forScope 1 true = .detachSession := by decide
example : stdioAccepts 2 = false := by decide

/-! ### Constants -/

/-- `PINNED_RETRY` in milliseconds: five `ELECTION_RETRY`s, never under a
second.
Pinned by `formal_constants_tests::registry`. -/
def pinnedRetry (er : Nat) : Nat := if 5 * er < 1000 then 1000 else 5 * er

/-- `PINNED_RETRY` is at least a second and at least `ELECTION_RETRY`
(test `the_pinned_retry_is_five_election_retries_and_at_least_a_second`): a
sub-second `ELECTION_RETRY` cannot make the retry loop spin. -/
theorem pinnedRetry_ge (er : Nat) :
    1000 ≤ pinnedRetry er ∧ 5 * er ≤ pinnedRetry er ∧ er ≤ pinnedRetry er := by
  unfold pinnedRetry
  split <;> omega

example : pinnedRetry 1000 = 5000 := by decide
example : pinnedRetry 0 = 1000 := by decide
example : pinnedRetry 150 = 1000 := by decide

/-- `unplaced_cap`: `UNPLACED_PER_PLACE × max_attached`, at least 1.
Pinned by `formal_constants_tests::registry`. -/
def unplacedCap (maxAttached : Nat) : Nat := max (maxAttached * 2) 1

/-- The registry's cap on unprobed on-demand attaches is at least one, so an
attach can always start. -/
theorem unplacedCap_pos (m : Nat) : 1 ≤ unplacedCap m := by
  unfold unplacedCap; omega

/-- The test `the_unplaced_cap_is_twice_max_attached_and_at_least_one`. -/
example : unplacedCap 16 = 32 ∧ unplacedCap 1 = 2 ∧ unplacedCap 0 = 1 := by decide

/-- `place_share` (`http_guards::credential_share`): the places divided
among the credentials, rounded down, at least 1. -/
def placeShare (places creds : Nat) : Nat := max (places / max creds 1) 1

/-- Every on-demand share of the registry's places is at least one. -/
theorem placeShare_pos (p n : Nat) : 1 ≤ placeShare p n := by
  unfold placeShare; omega

/-- Rounded down, the shares never add up to more than the places while there
are no more credentials than places (#32: "every credential can
always reach its share by eviction, whatever the others hold"). -/
theorem placeShare_fair {p n : Nat} (h1 : 1 ≤ n) (h2 : n ≤ p) : placeShare p n * n ≤ p := by
  unfold placeShare
  have hn : max n 1 = n := by omega
  rw [hn]
  have hq : 1 ≤ p / n := (Nat.le_div_iff_mul_le (by omega)).2 (by omega)
  have hm : max (p / n) 1 = p / n := by omega
  rw [hm]
  exact Nat.div_mul_le_self p n

/-- The test `the_place_share_is_pr5s_rule`. -/
example : placeShare 15 1 = 15 ∧ placeShare 15 2 = 7 ∧ placeShare 15 4 = 3 ∧
    placeShare 2 5 = 1 ∧ placeShare 0 3 = 1 := by decide

/-! ### `choose_victim` -/

/-- An on-demand slot as `choose_victim` reads it: its id, whether it is
pinned and live, its calls in flight, how long it has been idle (ms) and the
credential that attached it. -/
structure Cand where
  id : Nat
  pinned : Bool
  live : Bool
  inflight : Nat
  idle : Nat
  owner : Nat
  deriving DecidableEq, Repr

/-- `choose_victim`'s filter: live, not pinned, nothing in flight, idle at
least `min_idle`, and the requester's own, or, while the requester holds less
than its share, a session of a credential over its share. `held` is
`held_by`. -/
def eligible (held : Nat → Nat) (req share minIdle : Nat) (x : Cand) : Bool :=
  !x.pinned && x.live && x.inflight == 0 && decide (minIdle ≤ x.idle) &&
    (x.owner == req || (decide (held req < share) && decide (share < held x.owner)))

/-- `max_by(a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))`: `a` beats `b` when it
has been idle longer, or as long with a smaller id. -/
def beats (a b : Cand) : Bool := decide (b.idle < a.idle) || (a.idle == b.idle && decide (a.id < b.id))

def pick (acc : Option Cand) (x : Cand) : Option Cand :=
  match acc with
  | none => some x
  | some a => if beats x a = true then some x else some a

/-- `on_demand::choose_victim`. -/
def chooseVictim (held : Nat → Nat) (req share minIdle : Nat) (cs : List Cand) : Option Cand :=
  (cs.filter (eligible held req share minIdle)).foldl pick none

theorem foldl_pick_some : ∀ (l : List Cand) (a : Cand), ∃ v, l.foldl pick (some a) = some v
  | [], a => ⟨a, rfl⟩
  | x :: l, a => by
    simp only [List.foldl, pick]
    split
    · exact foldl_pick_some l x
    · exact foldl_pick_some l a

theorem foldl_pick_spec : ∀ (l : List Cand) (acc : Option Cand) (v : Cand),
    l.foldl pick acc = some v →
      (acc = some v ∨ v ∈ l) ∧ (∀ x ∈ l, x.idle ≤ v.idle) ∧ (∀ a, acc = some a → a.idle ≤ v.idle)
  | [], acc, v, h => by
    simp only [List.foldl] at h
    exact ⟨Or.inl h, by simp, fun a ha => by rw [h] at ha; cases ha; exact Nat.le_refl _⟩
  | x :: l, acc, v, h => by
    simp only [List.foldl] at h
    obtain ⟨hm, hl, ha⟩ := foldl_pick_spec l (pick acc x) v h
    have hx : ∀ r, pick acc x = some r → x.idle ≤ r.idle ∧ (∀ a, acc = some a → a.idle ≤ r.idle) ∧
        (r = x ∨ acc = some r) := by
      intro r hr
      unfold pick at hr
      split at hr
      · cases hr; exact ⟨Nat.le_refl _, (fun a e => by cases e), Or.inl rfl⟩
      · rename_i a
        split at hr
        · rename_i hb
          cases hr
          unfold beats at hb
          simp at hb
          refine ⟨Nat.le_refl _, (fun a' e => by cases e; omega), Or.inl rfl⟩
        · rename_i hb
          cases hr
          unfold beats at hb
          simp at hb
          refine ⟨by omega, (fun a' e => by cases e; exact Nat.le_refl _), Or.inr rfl⟩
    obtain ⟨r, hr⟩ : ∃ r, pick acc x = some r := by
      unfold pick; split
      · exact ⟨_, rfl⟩
      · split <;> exact ⟨_, rfl⟩
    obtain ⟨hxr, har, hor⟩ := hx r hr
    have hrv := ha r hr
    refine ⟨?_, ?_, ?_⟩
    · rcases hm with e | e
      · rw [hr] at e; cases e
        rcases hor with e' | e'
        · exact Or.inr (e' ▸ List.mem_cons_self)
        · exact Or.inl e'
      · exact Or.inr (List.mem_cons_of_mem _ e)
    · intro y hy
      rcases List.mem_cons.1 hy with e | e
      · subst e; omega
      · exact hl y e
    · intro a e
      have := har a e
      omega

/-- The victim is one of the candidates, and passes the filter: never pinned,
never one with a call in flight or used within `min_idle`. -/
theorem chooseVictim_eligible {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v : Cand} (h : chooseVictim held req share minIdle cs = some v) :
    v ∈ cs ∧ eligible held req share minIdle v = true := by
  obtain ⟨hm, -, -⟩ := foldl_pick_spec _ none v h
  rcases hm with e | e
  · cases e
  · exact List.mem_filter.1 e

/-- `Full` (503) exactly when no candidate passes the filter. -/
theorem chooseVictim_none_iff {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand} :
    chooseVictim held req share minIdle cs = none ↔
      ∀ x ∈ cs, eligible held req share minIdle x = false := by
  unfold chooseVictim
  constructor
  · intro h x hx
    cases e : eligible held req share minIdle x
    · rfl
    · exfalso
      have hmem : x ∈ cs.filter (eligible held req share minIdle) := List.mem_filter.2 ⟨hx, e⟩
      obtain ⟨l1, l2, hsplit⟩ := List.append_of_mem hmem
      rw [hsplit, List.foldl_append] at h
      simp only [List.foldl] at h
      obtain ⟨v, hv⟩ := foldl_pick_some l2 (match l1.foldl pick none with
        | none => x
        | some a => if beats x a = true then x else a)
      have : pick (l1.foldl pick none) x = some (match l1.foldl pick none with
          | none => x
          | some a => if beats x a = true then x else a) := by
        unfold pick; split
        · rfl
        · split <;> rfl
      rw [this, hv] at h
      cases h
  · intro h
    have : cs.filter (eligible held req share minIdle) = [] :=
      List.filter_eq_nil_iff.2 (fun x hx => by simp [h x hx])
    rw [this]; rfl

/-- The least recently used of the eligible: none of them has been idle
longer. -/
theorem chooseVictim_max {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v x : Cand} (h : chooseVictim held req share minIdle cs = some v) (hx : x ∈ cs)
    (he : eligible held req share minIdle x = true) : x.idle ≤ v.idle := by
  obtain ⟨-, hl, -⟩ := foldl_pick_spec _ none v h
  exact hl x (List.mem_filter.2 ⟨hx, he⟩)

/-- **A credential at or over its share evicts only its own sessions** (#32;
test `a_credential_at_its_share_evicts_only_its_own_sessions`). -/
theorem at_share_evicts_own {held : Nat → Nat} {req share minIdle : Nat} {cs : List Cand}
    {v : Cand} (hs : share ≤ held req) (h : chooseVictim held req share minIdle cs = some v) :
    v.owner = req := by
  have he := (chooseVictim_eligible h).2
  unfold eligible at he
  simp at he
  rcases he.2 with e | ⟨e, -⟩
  · exact e
  · omega

example : chooseVictim (fun _ => 1) 7 1 2000
    [⟨1, false, true, 0, 5000, 7⟩, ⟨2, false, true, 0, 9000, 8⟩, ⟨3, false, true, 0, 3000, 7⟩] =
    some ⟨1, false, true, 0, 5000, 7⟩ := by decide

example : chooseVictim (fun o => if o = 8 then 3 else 0) 7 1 2000
    [⟨1, false, true, 0, 5000, 7⟩, ⟨2, false, true, 0, 9000, 8⟩, ⟨4, true, true, 0, 99000, 7⟩,
     ⟨5, false, true, 1, 99000, 7⟩] =
    some ⟨2, false, true, 0, 9000, 8⟩ := by decide

/-! ### What a request gets: `answer_for` and `/admin/sessions` -/

/-- `Lookup`, with `Retry-After` in milliseconds. -/
inductive Lookup where
  | live
  | unavailable (retryAfter : Nat)
  | failed
  | erased
  | notHosted
  deriving DecidableEq, Repr

/-- `answer_for`, and `lookup`'s answer for a slot-less id. -/
def answer (pinned : Bool) (now : Nat) : Option Slot → Lookup
  | some (.live _) => .live
  | some (.attaching _ _) => .unavailable 1000
  | some (.detaching _ _) => .unavailable 1000
  | some (.held r _) => .unavailable (max (r - now) 1000)
  | some .failed => .failed
  | some (.erasing _ _) => .unavailable 1000
  | some .erased => .erased
  | none => if pinned then .unavailable 1000 else .notHosted

/-- Every 503 carries a `Retry-After` of at least a second, except `Failed`'s,
which has none (the table in the module docs). -/
theorem answer_retry_after_ge {pinned : Bool} {now r : Nat} {x : Option Slot}
    (h : answer pinned now x = .unavailable r) : 1000 ≤ r := by
  unfold answer at h
  split at h <;> (try split at h) <;> cases h <;> omega

/-- **A session still being erased is 503, never the erased 410** (#32;
test `an_erasing_session_is_503_and_only_an_erased_one_is_410`):
the permanent answer waits for the commit. -/
theorem answer_erasing (pinned : Bool) (now : Nat) (eh : Option H) (q : Prior) :
    answer pinned now (some (.erasing eh q)) = .unavailable 1000 := rfl

/-- Only a live slot is served, and a slot-less id is the uniform 404 only
when it is not pinned. -/
theorem answer_live_iff {pinned : Bool} {now : Nat} {x : Option Slot} :
    answer pinned now x = .live ↔ ∃ h, x = some (.live h) := by
  unfold answer
  split <;> simp
  split <;> simp

/-- `SlotView::state`. -/
def stateName : Option Slot → String
  | some (.attaching _ _) => "attaching"
  | some (.live _) => "live"
  | some (.detaching _ _) => "detaching"
  | some (.held _ _) => "held_elsewhere"
  | some .failed => "failed"
  | some (.erasing _ _) => "erasing"
  | some .erased => "erased"
  | none => "unattached"

/-- Insertion sort on ids, for the sorted on-demand rows. -/
def insertId (x : Nat) : List Nat → List Nat
  | [] => [x]
  | y :: l => if x ≤ y then x :: y :: l else y :: insertId x l

def sortIds : List Nat → List Nat
  | [] => []
  | x :: l => insertId x (sortIds l)

theorem mem_insertId {x y : Nat} : ∀ {l : List Nat}, y ∈ insertId x l ↔ y = x ∨ y ∈ l
  | [] => by simp [insertId]
  | z :: l => by
    unfold insertId
    split
    · simp
    · simp only [List.mem_cons, mem_insertId (l := l)]
      constructor
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)

theorem mem_sortIds {y : Nat} : ∀ {l : List Nat}, y ∈ sortIds l ↔ y ∈ l
  | [] => by simp [sortIds]
  | x :: l => by simp [sortIds, mem_insertId, mem_sortIds (l := l)]

/-- `slot_views`: a row for every pinned session in pinned order, then for
every other id holding a slot, sorted; each row is `(session, state, pinned)`. -/
def slotViews (c : Cfg) (f : Nat → Option Slot) : List (Nat × String × Bool) :=
  (c.pinned ++ sortIds (c.ids.filter (fun j => !isPinned c j && (f j).isSome))).map
    (fun j => (j, stateName (f j), isPinned c j))

/-- The pinned sessions come first, in pinned order. -/
theorem slotViews_pinned_first (c : Cfg) (f : Nat → Option Slot) :
    ((slotViews c f).map (·.1)).take c.pinned.length = c.pinned := by
  unfold slotViews
  simp [Function.comp_def]

/-- Every id the registry holds a slot for has its row, with its state. -/
theorem slotViews_complete {c : Cfg} {s : St} (hI : Inv c s) {j : Nat} {x : Slot}
    (hj : s.slots j = some x) : (j, stateName (some x), isPinned c j) ∈ slotViews c s.slots := by
  unfold slotViews
  rw [List.mem_map]
  refine ⟨j, ?_, by rw [hj]⟩
  rw [List.mem_append]
  by_cases hp : isPinned c j = true
  · left; simpa [isPinned] using hp
  · right
    rw [mem_sortIds, List.mem_filter]
    exact ⟨mem_of_some hI hj, by simp [hj]; simpa using hp⟩

/-! ### The negative cache and how an attach ends -/

/-- A create request never reads the cached `absent` answer for a session. -/
theorem negGet_create_not_absent (e : Option (Neg × Nat)) (now ttl : Nat) :
    negGet e now ttl true ≠ some .absent := by
  unfold negGet
  split
  · simp
  · split
    · simp
    · split <;> simp_all

/-- The registry's cached negative answer for a session expires once its TTL
has passed. -/
theorem negGet_expired (n : Neg) {t0 now ttl : Nat} (cr : Bool) (h : t0 + ttl ≤ now) :
    negGet (some (n, t0)) now ttl cr = none := by
  simp [negGet, h]

/-- The test `the_negative_cache_expires_skips_absent_for_create_and_is_bounded`
(its first half). -/
example : negGet (some (.erased, 0)) 10 30000 true = some .erased ∧
    negGet (some (.absent, 0)) 10 30000 false = some .absent ∧
    negGet (some (.absent, 0)) 10 30000 true = none ∧
    negGet (some (.erased, 0)) 30000 30000 false = none := by decide

/-- `AttachOutcome`. -/
inductive Outcome where
  | attached
  | absent
  | erased
  | busy
  | failed
  deriving DecidableEq, Repr

/-- `Flight::end`'s outcome: the drop guard's `Failed` on a slot already
live is `Attached` (#32). -/
def endOutcome (o : Outcome) (slotLive : Bool) : Outcome :=
  if o = .failed ∧ slotLive = true then .attached else o

/-- What `Flight::end` puts in the negative cache, if anything: `create` is
the flight's capability, `mine` whether its slot was still its own. -/
def negPut (o : Outcome) (create mine : Bool) : Option Neg :=
  match o with
  | .absent => if create = true then none else some .absent
  | .erased => some .erased
  | .failed => if mine = true then some .failed else none
  | _ => none

/-- Held elsewhere, transient errors, the closing serve and a timeout are never
cached: the next request tries again (#32). -/
theorem busy_never_cached (create mine : Bool) : negPut .busy create mine = none := rfl

/-- **A panic after admission does not block the session** (test
`a_panic_after_admission_does_not_block_the_session`): the drop guard finds
the slot live, its waiters are told `Attached`, and nothing is cached. -/
theorem panic_after_admit (create mine : Bool) :
    endOutcome .failed true = .attached ∧ negPut (endOutcome .failed true) create mine = none := by
  simp [endOutcome, negPut]

end Pure

end Lambo.Registry
