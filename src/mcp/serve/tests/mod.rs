//! Unit tests for `lambo serve`, grouped by subject.

use super::*;

mod heartbeat;
mod http_guards;
mod roles;
mod transport;

/// The fail-closed rule through the **real entry point**, not just the
/// helper: `serve` itself must refuse a non-loopback bind with no token.
///
/// Worth its own test because the unit test on [`authorize_bind`] proves the
/// rule and says nothing about whether anyone calls it — an edit that drops
/// the check from `serve` leaves that test green while shipping an
/// unauthenticated writer to the world.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod refuses_to_start;

/// **P2-a.** The `ResolvedBackends.config` → `Memory` seam on the *serve*
/// path, pinned where `lambo serve` actually crosses it.
///
/// The chain — `load_resolved` → `resolve_backends` → `ResolvedBackends.config`
/// → `serve_builder`'s `.config(..)` → `Memory::build` →
/// `CanonizationTask::from_daemon` → `EvalParams::from_config` →
/// `promotion_policy.scorer()` — is correct, and until this test nothing
/// held either of its two middle links down. `src/resolve.rs` pins file →
/// `ResolvedBackends.config` and `src/mcp/server.rs` pins `Config` → live
/// canonization; deleting `.config(config)` from [`serve_builder`] left
/// every other test in the suite green while `lambo serve` silently
/// reverted to `Swarm`.
///
/// This is not hypothetical. `open_writer` shipped exactly this regression
/// (T1-R1-2) and carries the sibling test —
/// `open_writer_forwards_resolved_config_daemon_overrides` in
/// `src/cli/mod.rs` — for the same reason. `serve` is the other consumer of
/// the same field and had no equivalent.
///
/// Both policies are asserted: a `serve_builder` that hardcoded either one
/// would pass a single-arm test.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod carries_the_resolved_config;

/// **Issue #13 review (pre-existing defect).** A serve that loses the
/// election and becomes a proxy must not keep the embedder it resolved.
///
/// The real shape, in-process: a holder `Memory` takes the lease on a
/// file-backed SQLite store, publishing an endpoint it then binds, so
/// [`resolve_role`] for a second agent sees a reachable holder and returns
/// [`Role::Proxy`]. The second builder's embedder carries a drop flag. With
/// the proxy still alive, the flag must already be set: nothing on the
/// proxy path (the role, the store handle it re-reads the lease from, the
/// builder `serve` used to keep across the arm) may hold the model. With
/// candle on Metal that was ~1.1 GB of weights per proxying client.
#[cfg(all(unix, feature = "store-sqlite", feature = "embed-fixture"))]
mod proxy_releases_the_model;

/// **Test-gap (a) pinned.** `run_and_close` is the seam that guarantees
/// [`Memory::close`] runs on *every* transport exit path. This drives it
/// with a transport that returns `Ok` and one that returns `Err`, and after
/// each asserts the session is genuinely closed — a synchronous write is now
/// refused — so a future edit that skips the close on one branch fails here
/// rather than in production as a silently-dropped tail.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod close_runs;
