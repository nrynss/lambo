//! The writers gate (T81-1, COH-6 clause 14): how a write and `close()`
//! exclude each other.
//!
//! Every mutating method holds a **read** permit on `Memory::writers` for its
//! whole body, awaits included, and re-checks `closed` and the lease fence after
//! acquiring it. `close()` latches `closed`, drains the write queue, and then
//! takes the **write** side, which waits out every write already in flight. So
//! a write either finishes and lands in close's final batch, or is refused; it
//! is never acknowledged and lost.
//!
//! The gate is a `tokio` RwLock because writes hold it across `.await`; it is
//! not the graph lock, which is still never held across an `.await` (§6.4).
//! Read-only methods only call [`Memory::ensure_open`] and never take the gate,
//! so a long recall cannot delay shutdown. The write queue's workers never take
//! the gate either (see `writeq/drain.rs` for why).

use std::sync::atomic::Ordering;

use tokio::sync::RwLockReadGuard as AsyncRwLockReadGuard;

use super::Memory;
use crate::types::LamboError;

impl Memory {
    pub(super) fn ensure_open(&self) -> Result<(), LamboError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        Ok(())
    }

    pub(super) fn closed_error(&self) -> LamboError {
        LamboError::Config(format!("session {} is closed", self.session))
    }

    /// Enter the writers gate from an **async** method (T81-1).
    ///
    /// [`Memory::ensure_open`] first, so a write against an already-closed
    /// session is refused without queueing behind `close()`'s write side; then
    /// the read permit; then the check **again**, because `close()` may have
    /// latched `closed` while this call waited for the permit.
    ///
    /// The second check is what makes the gate airtight. If it sees `closed`
    /// as open, the latch had not happened yet, so `close()`'s later
    /// `writers.write()` must wait for the permit this returns — the write
    /// completes and its mutations are in `close()`'s final batch. If it sees
    /// `closed`, the write is refused and never touched the graph.
    pub(super) async fn begin_write(&self) -> Result<AsyncRwLockReadGuard<'_, ()>, LamboError> {
        self.ensure_open()?;
        // T86-2: a fenced handle (lost its lease) refuses before it touches the
        // gate — the strongest refusal, checked first.
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        let permit = self.writers.read().await;
        self.ensure_open()?;
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        Ok(permit)
    }

    /// Enter the writers gate from a **synchronous** method.
    ///
    /// `try_read` rather than `read().await`: these methods cannot await, and
    /// `blocking_read` on a runtime worker would be worse than the race it
    /// fixes. The only thing that holds the write side is `close()`, so a
    /// failed `try_read` means exactly "a close is in progress" and maps to the
    /// closed error. The post-acquire re-check is the same barrier as
    /// [`Memory::begin_write`]'s — and since a sync method never awaits, the
    /// permit is held continuously from the check to the last mutation, so
    /// `close()` cannot drain past it.
    ///
    /// **Coverage note (R2-6).** The `try_read` refusal is pinned by
    /// `a_sync_write_is_refused_while_the_gate_is_taken`; the re-check itself is
    /// not, and cannot honestly be. Its window — `try_read` *succeeding* after
    /// `close()` latched but before `close()` requests the write side — is one
    /// instruction wide and needs true parallelism to enter, so no
    /// deterministic single-threaded interleaving reaches it and a probabilistic
    /// hammer would not fail reliably either. It is kept because it costs an
    /// atomic load and closes the same hole `begin_write`'s does — where the
    /// window is wide enough to construct, and *is* constructed, by
    /// `a_write_that_takes_the_gate_after_close_latched_is_refused`. Same
    /// blind-spot class as T81-4's `biased;`.
    pub(super) fn begin_write_sync(&self) -> Result<AsyncRwLockReadGuard<'_, ()>, LamboError> {
        self.ensure_open()?;
        // T86-2: fenced handles refuse before touching the gate (see `begin_write`).
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        let permit = self.writers.try_read().map_err(|_| self.closed_error())?;
        self.ensure_open()?;
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        Ok(permit)
    }
}
