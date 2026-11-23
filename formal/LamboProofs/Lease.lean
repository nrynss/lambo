/-
# The writer lease: one token generation at a time

Model of the session lease (`src/store/lease.rs`, and the `MemoryStore` upsert
in `src/store/memory.rs`, which states the same rule as the SQL adapters'
single `INSERT ... ON CONFLICT ... WHERE expires_at <= now OR holder = me`).

A lease row carries a holder identity, an expiry and a fencing token. A writer
captures the token it was given at acquire time and presents it on every flush
and canonization write; the store admits the write only if
`lease_permits_write(current_token, presented)` holds inside that write's own
transaction.

## Model

* `Row`: the lease row of one session, once one exists.
* `acquire me now ttl r`: the upsert. A live row held by someone else is left
  untouched (`Held`); the holder's own row is refreshed and **keeps** its token;
  otherwise (expired, or released) the caller takes over and the token is
  **incremented**.
* `release me now r`: holder-scoped; the row is kept, its holder becomes the
  reserved `released` marker and it expires at once. The token is not reset.
* `released` is the reserved holder `"lambo:released"`. `refuse_reserved_holder`
  rejects it as a caller identity, which is the side condition on `Step.acq`.
* `Permits current presented`: `lease_permits_write`.

Time is an integer and is supplied to each operation by the store, never by a
client. Nothing below assumes it moves forward.

Not modelled: the erased-session tombstone, and the first acquire on a session
with no row (it mints token `1`; `first` below is that row).

## Results

* `token_mono`: over any sequence of acquires and releases the token never
  decreases ("`current_token` only ever goes up").
* `acquire_refused_while_live`: while a lease is live, another identity's
  acquire changes nothing.
* `takeover_needs_expiry`: an identity that did not hold the row gets it only
  when `expires_at <= now`.
* `stale_token_refused`: once any other identity holds the lease, the previous
  holder's captured token no longer passes the fence, and by `Steps` this stays
  true forever ("a pre-takeover holder's stale token can never pass").
* `holder_token_permitted`: the current holder's captured token does pass.

The last two are the single-writer guarantee at the store: of all the tokens
ever handed out for a session, only the newest generation can write.

## The heartbeat (#293)

The heartbeat in `src/memory/leases.rs` (`spawn_lease_heartbeat`) refreshes
the lease with an `acquire` by the holder and decides from the answer whether
to latch the lease-lost fence. `beatLatchesOld` is the behaviour before #293:
it latches only on `Held`, i.e. when another identity holds the row after the
refresh, and ignores the token an `Acquired` returns. `beatLatches` is the
fix: it also latches when the refresh comes back `Acquired` at a token other
than the one the handle captured at its own acquire.

* `beat_latches_iff`: over any history, the fixed beat latches exactly when
  the row is no longer the holder's own generation (held by it at its
  captured token). So the first beat after any takeover latches, whoever holds
  the row by then, the holder itself included (a re-take).
* `beat_own_generation`: on the holder's own generation neither beat latches;
  the fix changes nothing there. `beatLatchesOld_le`: every old latch is still
  a latch.
* `override_beat_latches`: after the documented operator override (a release
  by hand), the old beat re-takes the row and does not latch; the fixed beat
  latches.
-/

namespace Lambo.Lease

/-- One session's lease row. `H` is the holder-identity type. -/
structure Row (H : Type) where
  holder : H
  token : Nat
  expires : Int

variable {H : Type} [DecidableEq H]

/-- The row written by the first acquire on a session: token `1`. -/
def first (me : H) (now ttl : Int) : Row H :=
  { holder := me, token := 1, expires := now + ttl }

/-- The atomic acquire/refresh upsert. -/
def acquire (me : H) (now ttl : Int) (r : Row H) : Row H :=
  if now < r.expires ∧ r.holder ≠ me then r
  else if r.holder = me then { holder := r.holder, token := r.token, expires := now + ttl }
  else { holder := me, token := r.token + 1, expires := now + ttl }

/-- Holder-scoped release: expire the row, keep the token. -/
def release (released : H) (me : H) (now : Int) (r : Row H) : Row H :=
  if r.holder = me then { holder := released, token := r.token, expires := now } else r

/-- `lease_permits_write`: an unleased session (`current = 0`) admits any
write; otherwise the writer must present a token at least the current one. -/
def Permits (current : Nat) (presented : Option Nat) : Prop :=
  current = 0 ∨ ∃ p, presented = some p ∧ current ≤ p

/-! ## The three arms of the upsert -/

section Arms

variable {me : H} {now ttl : Int} {r : Row H}

/-- `Held`: a live lease held by someone else is left exactly as it was. -/
theorem acquire_held (h : now < r.expires ∧ r.holder ≠ me) : acquire me now ttl r = r := by
  unfold acquire
  exact ite_eq_left h

/-- Refresh: the holder's own row keeps its token and gets a new expiry. This
arm applies even to the holder's own *expired* row. -/
theorem acquire_refresh (h : r.holder = me) :
    acquire me now ttl r = { holder := r.holder, token := r.token, expires := now + ttl } := by
  have hn : ¬ (now < r.expires ∧ r.holder ≠ me) := fun hc => hc.2 h
  unfold acquire
  rw [ite_eq_right hn, ite_eq_left h]

/-- Takeover: an expired (or released) row held by another identity passes to
the caller with the token incremented. -/
theorem acquire_takeover (h1 : ¬ (now < r.expires ∧ r.holder ≠ me)) (h2 : r.holder ≠ me) :
    acquire me now ttl r = { holder := me, token := r.token + 1, expires := now + ttl } := by
  unfold acquire
  rw [ite_eq_right h1, ite_eq_right h2]

theorem release_mine {released : H} (h : r.holder = me) :
    release released me now r = { holder := released, token := r.token, expires := now } := by
  unfold release
  exact ite_eq_left h

theorem release_other {released : H} (h : r.holder ≠ me) : release released me now r = r := by
  unfold release
  exact ite_eq_right h

end Arms

/-! ## Local facts about one operation -/

/-- An acquire never lowers the token. -/
theorem acquire_token_ge (me : H) (now ttl : Int) (r : Row H) :
    r.token ≤ (acquire me now ttl r).token := by
  by_cases hheld : now < r.expires ∧ r.holder ≠ me
  · rw [acquire_held hheld] <;> exact Nat.le_refl r.token
  · by_cases hme : r.holder = me
    · rw [acquire_refresh hme] <;> exact Nat.le_refl r.token
    · rw [acquire_takeover hheld hme] <;> exact Nat.le_succ r.token

/-- A release never changes the token ("a release keeps the fencing token"). -/
theorem release_token (released me : H) (now : Int) (r : Row H) :
    (release released me now r).token = r.token := by
  by_cases hme : r.holder = me
  · rw [release_mine hme] <;> rfl
  · rw [release_other hme]

/-- **Refusal while live.** While the lease has not expired, an acquire by any
other identity leaves the row untouched: the caller does not become holder. -/
theorem acquire_refused_while_live (me : H) (now ttl : Int) (r : Row H)
    (hlive : now < r.expires) (hother : r.holder ≠ me) :
    acquire me now ttl r = r ∧ (acquire me now ttl r).holder ≠ me := by
  have e := acquire_held (ttl := ttl) ⟨hlive, hother⟩
  rw [e]
  exact ⟨rfl, hother⟩

/-- **Takeover needs expiry.** If the caller did not hold the row before and
holds it after, the row had expired: `expires_at <= now`. The boundary
`expires_at == now` counts as expired. -/
theorem takeover_needs_expiry (me : H) (now ttl : Int) (r : Row H)
    (hbefore : r.holder ≠ me) (hafter : (acquire me now ttl r).holder = me) :
    r.expires ≤ now := by
  by_cases hlive : now < r.expires
  · rw [acquire_held ⟨hlive, hbefore⟩] at hafter
    exact absurd hafter hbefore
  · omega

/-- A takeover mints a strictly larger token; a same-holder refresh preserves
it. -/
theorem acquire_token_cases (me : H) (now ttl : Int) (r : Row H)
    (hafter : (acquire me now ttl r).holder = me) :
    (r.holder = me ∧ (acquire me now ttl r).token = r.token) ∨
    (r.holder ≠ me ∧ (acquire me now ttl r).token = r.token + 1) := by
  by_cases hme : r.holder = me
  · left
    rw [acquire_refresh hme]
    exact ⟨hme, rfl⟩
  · right
    by_cases hheld : now < r.expires ∧ r.holder ≠ me
    · rw [acquire_held hheld] at hafter
      exact absurd hafter hme
    · rw [acquire_takeover hheld hme]
      exact ⟨hme, rfl⟩

/-! ## Histories -/

/-- One lease operation. A caller identity is never the reserved `released`
marker (`refuse_reserved_holder`). -/
inductive Step (released : H) : Row H → Row H → Prop
  | acq (me : H) (now ttl : Int) (r : Row H) :
      me ≠ released → Step released r (acquire me now ttl r)
  | rel (me : H) (now : Int) (r : Row H) :
      Step released r (release released me now r)

/-- Any finite history of lease operations. -/
inductive Steps (released : H) : Row H → Row H → Prop
  | refl (r : Row H) : Steps released r r
  | tail {a b c : Row H} : Steps released a b → Step released b c → Steps released a c

theorem step_token_mono {released : H} {a b : Row H} (h : Step released a b) :
    a.token ≤ b.token := by
  cases h with
  | acq me now ttl r _ => exact acquire_token_ge _ _ _ _
  | rel me now r => rw [release_token] <;> exact Nat.le_refl _

/-- **The token only goes up.** -/
theorem token_mono {released : H} {a b : Row H} (h : Steps released a b) :
    a.token ≤ b.token := by
  induction h with
  | refl => exact Nat.le_refl _
  | tail _ hstep ih => exact Nat.le_trans ih (step_token_mono hstep)

/-! ## The fence -/

/-- What a writer `A` holding captured token `t` can still rely on: either the
row is still at generation `t` and is `A`'s (or `A` released it and nobody has
taken it since), or the row has moved to a later generation. -/
def Safe (released A : H) (t : Nat) (r : Row H) : Prop :=
  (r.token = t ∧ (r.holder = A ∨ r.holder = released)) ∨ t < r.token

theorem safe_acquire {released A : H} {t : Nat} {r : Row H} (me : H) (now ttl : Int)
    (h : Safe released A t r) :
    Safe released A t (acquire me now ttl r) := by
  unfold Safe at h ⊢
  by_cases hheld : now < r.expires ∧ r.holder ≠ me
  · rw [acquire_held hheld]
    exact h
  · by_cases hm : r.holder = me
    · rw [acquire_refresh hm]
      exact h
    · rw [acquire_takeover hheld hm]
      cases h with
      | inl h =>
        have ht : r.token = t := h.1
        exact Or.inr (by show t < r.token + 1; omega)
      | inr h => exact Or.inr (by show t < r.token + 1; omega)

theorem safe_release {released A : H} {t : Nat} {r : Row H} (me : H) (now : Int)
    (h : Safe released A t r) : Safe released A t (release released me now r) := by
  unfold Safe at h ⊢
  by_cases hm : r.holder = me
  · rw [release_mine hm]
    cases h with
    | inl h => exact Or.inl ⟨h.1, Or.inr rfl⟩
    | inr h => exact Or.inr h
  · rw [release_other hm]
    exact h

theorem safe_step {released A : H} {t : Nat} {a b : Row H} (hstep : Step released a b)
    (h : Safe released A t a) : Safe released A t b := by
  cases hstep with
  | acq me now ttl r _ => exact safe_acquire me now ttl h
  | rel me now r => exact safe_release me now h

theorem safe_steps {released A : H} {t : Nat} {a b : Row H} (hsteps : Steps released a b)
    (h : Safe released A t a) : Safe released A t b := by
  induction hsteps with
  | refl => exact h
  | tail _ hstep ih => exact safe_step hstep ih

omit [DecidableEq H] in
/-- A writer that has just become holder is `Safe` at the token it captured. -/
theorem safe_of_holder (released : H) (r : Row H) : Safe released r.holder r.token r := by
  unfold Safe
  exact Or.inl ⟨rfl, Or.inl rfl⟩

/-- **A stale token never passes.** Writer `A` captured token `t` while holding
the row `a`. After any history, if the row is held by some *other* live
identity, then `A`'s token is refused by the fence. By `Steps` this covers
every later moment too: the token can only move further away. -/
theorem stale_token_refused {released A : H} {a b : Row H}
    (hheld : a.holder = A) (hsteps : Steps released a b)
    (hother : b.holder ≠ A) (hlive : b.holder ≠ released) :
    ¬ Permits b.token (some a.token) := by
  have hsafe : Safe released A a.token b := by
    apply safe_steps hsteps
    rw [← hheld]
    exact safe_of_holder released a
  unfold Safe at hsafe
  have hlt : a.token < b.token := by
    cases hsafe with
    | inl h =>
      cases h.2 with
      | inl h2 => exact absurd h2 hother
      | inr h2 => exact absurd h2 hlive
    | inr h => exact h
  intro hp
  unfold Permits at hp
  cases hp with
  | inl h0 => omega
  | inr hex =>
    cases hex with
    | intro p hp' =>
      have e : a.token = p := Option.some.inj hp'.1
      have hle : b.token ≤ p := hp'.2
      omega

omit [DecidableEq H] in
/-- The other half: the identity that holds the row passes the fence with the
token it was given, for as long as the row is still its own generation. -/
theorem holder_token_permitted (r : Row H) : Permits r.token (some r.token) := by
  unfold Permits
  exact Or.inr ⟨r.token, rfl, Nat.le_refl r.token⟩

/-- Once a session is leased, the writer fence refuses a writer that presents
no lease token. -/
theorem no_token_refused (current : Nat) (h : 0 < current) : ¬ Permits current none := by
  intro hp
  unfold Permits at hp
  cases hp with
  | inl h0 => omega
  | inr hex =>
    cases hex with
    | intro p hp' =>
      have hnone : (none : Option Nat) = some p := hp'.1
      cases hnone

/-- The writer fence admits any write to a session that was never leased: the
seed and fixture bypass. -/
theorem unleased_permits (presented : Option Nat) : Permits 0 presented := by
  unfold Permits
  exact Or.inl rfl

/-- **Two generations never both pass.** If two distinct identities each
captured a token at their own acquire, and the second acquired after the
first, then at the second's generation the first's token is refused while the
second's passes. -/
theorem later_holder_excludes_earlier {released A B : H} {a b : Row H}
    (hA : a.holder = A) (hB : b.holder = B) (hAB : B ≠ A) (hBlive : B ≠ released)
    (hsteps : Steps released a b) :
    ¬ Permits b.token (some a.token) ∧ Permits b.token (some b.token) := by
  refine ⟨?_, holder_token_permitted b⟩
  apply stale_token_refused hA hsteps
  · rw [hB]
    exact hAB
  · rw [hB]
    exact hBlive

/-- After a release, the next acquire by *any* identity, the releaser
included, mints a new generation: a handle that released cannot resume writing
under its old token once anyone has acquired. -/
theorem acquire_after_release_bumps (released me who : H) (now now' ttl : Int) (r : Row H)
    (hme : r.holder = me) (hwho : who ≠ released) (hexp : now ≤ now') :
    (acquire who now' ttl (release released me now r)).token = r.token + 1 := by
  rw [release_mine hme]
  have h2 : ({ holder := released, token := r.token, expires := now } : Row H).holder ≠ who :=
    fun e => hwho e.symm
  have h1 : ¬ (now' < ({ holder := released, token := r.token, expires := now } : Row H).expires
      ∧ ({ holder := released, token := r.token, expires := now } : Row H).holder ≠ who) := by
    intro hc
    have hlt : now' < now := hc.1
    omega
  rw [acquire_takeover h1 h2] <;> rfl

/-! ## The heartbeat's latch decision (#293) -/

/-- The heartbeat before #293: refresh (an `acquire` by the holder `me`) and
latch the fence only when the answer is `Held`, i.e. another identity holds
the row after the refresh (`Ok(LeaseOutcome::Acquired(_)) => {}`). -/
def beatLatchesOld (me : H) (now ttl : Int) (r : Row H) : Bool :=
  decide ((acquire me now ttl r).holder ≠ me)

/-- The fixed heartbeat: latch on `Held`, and also when the refresh comes back
`Acquired` at a token other than `t`, the one the handle captured at its own
acquire (and presents on every flush). -/
def beatLatches (me : H) (t : Nat) (now ttl : Int) (r : Row H) : Bool :=
  decide ((acquire me now ttl r).holder ≠ me) || decide ((acquire me now ttl r).token ≠ t)

/-- The #293 fix to the heartbeat's latch only adds latches: every beat the
old rule latched still latches. -/
theorem beatLatchesOld_le (me : H) (t : Nat) (now ttl : Int) (r : Row H)
    (h : beatLatchesOld me now ttl r = true) : beatLatches me t now ttl r = true := by
  unfold beatLatchesOld at h
  unfold beatLatches
  rw [h]
  rfl

/-- **On the holder's own generation neither beat latches.** While the row is
`me`'s at the token `me` captured, the refresh keeps both, so the fix changes
nothing for a healthy holder. -/
theorem beat_own_generation {me : H} {t : Nat} (now ttl : Int) {r : Row H}
    (hh : r.holder = me) (ht : r.token = t) :
    beatLatches me t now ttl r = false ∧ beatLatchesOld me now ttl r = false := by
  have e := acquire_refresh (now := now) (ttl := ttl) hh
  unfold beatLatches beatLatchesOld
  rw [e]
  simp [hh, ht]

/-- **The fixed beat latches exactly when the row has left the holder's
generation.** Writer `A` captured token `a.token` at row `a`. After any history
of acquires and releases (`Steps`), `A`'s next beat latches if and only if the
row is not `A`'s at that token: another identity holds it (`Held`), the row was
released (by `A`, or by the operator override), or `A` itself holds it at a
later token (a re-take). Since this holds at every row after a takeover, the
fence latches at the first beat after it, within one
`LEASE_HEARTBEAT_INTERVAL`. -/
theorem beat_latches_iff {released A : H} {a b : Row H} (hsteps : Steps released a b)
    (now ttl : Int) :
    beatLatches A a.token now ttl b = true ↔ ¬ (b.holder = A ∧ b.token = a.token) := by
  constructor
  · intro hl hown
    rw [(beat_own_generation now ttl hown.1 hown.2).1] at hl
    cases hl
  · intro hnot
    have hmono : a.token ≤ b.token := token_mono hsteps
    unfold beatLatches
    by_cases hA : (acquire A now ttl b).holder = A
    · rcases acquire_token_cases A now ttl b hA with ⟨hb, htok⟩ | ⟨_, htok⟩
      · have hne : b.token ≠ a.token := fun e => hnot ⟨hb, e⟩
        simp [htok, hne]
      · have hne : b.token + 1 ≠ a.token := by omega
        simp [htok, hne]
    · simp [hA]

/-- `spawn_lease_heartbeat`'s beat latches the fence on the first beat after
any takeover: once the token has moved past the one the holder captured, its
beat latches whoever holds the row. -/
theorem beat_latches_after_takeover {released A : H} {a b : Row H}
    (hsteps : Steps released a b) (hlt : a.token < b.token) (now ttl : Int) :
    beatLatches A a.token now ttl b = true :=
  (beat_latches_iff hsteps now ttl).mpr (fun h => by omega)

/-- **The operator-override re-take.** The override releases the wedged
holder `A`'s row by hand (holder `lambo:released`, expired, token kept). `A`'s
next refresh takes it over at `token + 1`, holder `A` again: the old beat sees
`Acquired` and does not latch; the fixed beat sees the token move and does. -/
theorem override_beat_latches {released A : H} (a : Row H) (hA : a.holder = A)
    (hAr : A ≠ released) {now now' : Int} (hle : now ≤ now') (ttl : Int) :
    beatLatchesOld A now' ttl (release released A now a) = false ∧
    beatLatches A a.token now' ttl (release released A now a) = true := by
  have htok := acquire_after_release_bumps released A A now now' ttl a hA hAr hle
  have hhold : (acquire A now' ttl (release released A now a)).holder = A := by
    rw [release_mine hA]
    have h2 : ({ holder := released, token := a.token, expires := now } : Row H).holder ≠ A :=
      fun e => hAr e.symm
    have h1 : ¬ (now' < ({ holder := released, token := a.token, expires := now } : Row H).expires
        ∧ ({ holder := released, token := a.token, expires := now } : Row H).holder ≠ A) := by
      intro hc
      have : now' < now := hc.1
      omega
    rw [acquire_takeover h1 h2]
  unfold beatLatchesOld beatLatches
  simp [hhold, htok]

/-- The override scenario concretely (the `exRetakenRow` of `Flush.lean`):
holder `0` holds token 1, the override at 50 s releases it, the refresh at
60 s re-takes it at token 2. The old beat does not latch; the fixed one does. -/
example :
    beatLatchesOld 0 60000 45000 (release 99 0 50000 (first 0 0 45000)) = false ∧
    beatLatches 0 1 60000 45000 (release 99 0 50000 (first 0 0 45000)) = true := by
  decide

/-- A healthy holder's beat latches under neither version. -/
example :
    beatLatchesOld 0 15000 45000 (first 0 0 45000) = false ∧
    beatLatches 0 1 15000 45000 (first 0 0 45000) = false := by
  decide

end Lambo.Lease
