/-
# Soft reservations: one live holder per node

Model of `reserve`, `release` and `active_reservation` in
`src/graph/reserve.rs`. A node carries at most one reservation
(`set_reservation` replaces by node id), so the state for one node is an
optional `(agent, expires_at)` pair.

Reservations are advisory: they produce a "Reserved by" warning on recall and
"never block writes". What is proved here is the lock discipline itself.

## Results

* `reserve_denied`: while another agent's reservation is live, a reserve
  attempt fails and leaves the reservation untouched.
* `takeover_needs_expiry`: an agent takes a node from another agent only at or
  after the expiry instant (`now >= expires_at`; the lock is half-open).
* `reserve_holder`: a successful reserve makes the caller the holder, with
  expiry `now + ttl`.
* `active_iff`: "active iff `now < expires_at`".
* `release_owner_only`: only the holder can release, and it can release its
  own expired lock.
-/

namespace Lambo.Reserve

/-- A reservation on one node. -/
structure Reservation (A : Type) where
  agent : A
  expires : Int

variable {A : Type} [DecidableEq A]

/-- `reserve`: the new state and whether the call succeeded. -/
def reserve (agent : A) (now ttl : Int) : Option (Reservation A) → Option (Reservation A) × Bool
  | none => (some { agent := agent, expires := now + ttl }, true)
  | some r =>
    if r.agent = agent then (some { agent := agent, expires := now + ttl }, true)
    else if now < r.expires then (some r, false)
    else (some { agent := agent, expires := now + ttl }, true)

/-- `release`: the new state and whether the call succeeded. -/
def release (agent : A) : Option (Reservation A) → Option (Reservation A) × Bool
  | none => (none, false)
  | some r => if r.agent = agent then (none, true) else (some r, false)

/-- `active_reservation`: the reservation, unless it has expired. -/
def active (now : Int) : Option (Reservation A) → Option (Reservation A)
  | none => none
  | some r => if now < r.expires then some r else none

/-- **Cross-agent denied while live.** -/
theorem reserve_denied (agent : A) (now ttl : Int) (r : Reservation A)
    (hother : r.agent ≠ agent) (hlive : now < r.expires) :
    reserve agent now ttl (some r) = (some r, false) := by
  show (if r.agent = agent then (some { agent := agent, expires := now + ttl }, true)
    else if now < r.expires then (some r, false)
    else (some { agent := agent, expires := now + ttl }, true)) = (some r, false)
  rw [if_neg hother, if_pos hlive]

/-- **Takeover needs expiry.** If a reserve succeeds against another agent's
reservation, that reservation had expired. The boundary `now == expires_at`
counts as expired (Rust test `takeover_at_exact_expiry_boundary`). -/
theorem takeover_needs_expiry (agent : A) (now ttl : Int) (r : Reservation A)
    (hother : r.agent ≠ agent) (hok : (reserve agent now ttl (some r)).2 = true) :
    r.expires ≤ now := by
  by_cases hlive : now < r.expires
  · rw [reserve_denied agent now ttl r hother hlive] at hok
    have hok' : false = true := hok
    exact absurd hok' (by decide)
  · omega

/-- A successful reserve makes the caller the holder until `now + ttl`. -/
theorem reserve_holder (agent : A) (now ttl : Int) (s : Option (Reservation A))
    (hok : (reserve agent now ttl s).2 = true) :
    (reserve agent now ttl s).1 = some { agent := agent, expires := now + ttl } := by
  cases s with
  | none => rfl
  | some r =>
    by_cases hsame : r.agent = agent
    · have e : reserve agent now ttl (some r)
          = (some { agent := agent, expires := now + ttl }, true) := by
        show (if r.agent = agent then (some { agent := agent, expires := now + ttl }, true)
          else if now < r.expires then (some r, false)
          else (some { agent := agent, expires := now + ttl }, true)) = _
        rw [if_pos hsame]
      rw [e] <;> rfl
    · by_cases hlive : now < r.expires
      · rw [reserve_denied agent now ttl r hsame hlive] at hok
        have hok' : false = true := hok
        exact absurd hok' (by decide)
      · have e : reserve agent now ttl (some r)
            = (some { agent := agent, expires := now + ttl }, true) := by
          show (if r.agent = agent then (some { agent := agent, expires := now + ttl }, true)
            else if now < r.expires then (some r, false)
            else (some { agent := agent, expires := now + ttl }, true)) = _
          rw [if_neg hsame, if_neg hlive]
        rw [e] <;> rfl

/-- The same agent always succeeds: it extends (or shortens) its own lock, even
an expired one. -/
theorem reserve_same_agent (agent : A) (now ttl : Int) (r : Reservation A)
    (hsame : r.agent = agent) :
    reserve agent now ttl (some r) = (some { agent := agent, expires := now + ttl }, true) := by
  show (if r.agent = agent then (some { agent := agent, expires := now + ttl }, true)
    else if now < r.expires then (some r, false)
    else (some { agent := agent, expires := now + ttl }, true)) = _
  rw [if_pos hsame]

/-- "Active iff `now < expires_at`". -/
theorem active_iff (now : Int) (r : Reservation A) :
    active now (some r) = some r ↔ now < r.expires := by
  constructor
  · intro h
    by_cases hlive : now < r.expires
    · exact hlive
    · have e : active now (some r) = none := by
        show (if now < r.expires then some r else none) = none
        rw [if_neg hlive]
      rw [e] at h
      exact Option.noConfusion h
  · intro hlive
    show (if now < r.expires then some r else none) = some r
    rw [if_pos hlive]

/-- **One live holder.** While a reservation is active, no other agent can
obtain the node: its reserve fails and the active reservation is unchanged. -/
theorem active_excludes_others (agent : A) (now ttl : Int) (r : Reservation A)
    (hactive : active now (some r) = some r) (hother : r.agent ≠ agent) :
    reserve agent now ttl (some r) = (some r, false) :=
  reserve_denied agent now ttl r hother ((active_iff now r).mp hactive)

/-- Only the holder can release; a release by anyone else fails and changes
nothing. The holder can release whether or not its lock has expired (Rust test
`release_of_expired_lock_is_identity_only`). -/
theorem release_owner_only (agent : A) (r : Reservation A) :
    (r.agent = agent → release agent (some r) = (none, true)) ∧
    (r.agent ≠ agent → release agent (some r) = (some r, false)) := by
  constructor
  · intro h
    show (if r.agent = agent then (none, true) else (some r, false)) = (none, true)
    rw [if_pos h]
  · intro h
    show (if r.agent = agent then (none, true) else (some r, false)) = (some r, false)
    rw [if_neg h]

end Lambo.Reserve
