//! Erasing a session from a running serve (#32 PR 7, design §6.3): the
//! deferred #23 surface, in the process that holds the session.
//!
//! [`SessionRegistry::erase`] is the one entry point; the admin route
//! (`super::super::admin`) authorizes the caller and checks the confirm
//! before it is reached, so nothing here sees an unauthorized request.
//!
//! # An attached session: fence, quiesce, then erase as the holder
//!
//! 1. The slot becomes [`Slot::Erasing`]: new requests get the erased
//!    refusal and no attach or background retry starts for the id.
//! 2. The session's lease watcher is stopped, so the fence below is not
//!    booked as a lost lease and does not spawn a detach.
//! 3. The handle is fenced **in this process only**
//!    ([`Memory::fence_for_erase`]): every read and write of it is refused
//!    with the #23 erased error from here, and its close will neither flush
//!    nor release the lease. The lease row is untouched, so it stays this
//!    process's and no other writer can acquire the session.
//! 4. Its MCP sessions end (the detach's stage 1), it is closed (stages 3
//!    and 4: the write pipeline quiesced, the writers gate drained, every
//!    background task aborted **and joined**, the in-RAM tail discarded),
//!    its watcher stopped (5) and its endpoint released (6).
//! 5. `store.erase_session(id, mem.lease_holder())`: the #23 gate admits
//!    the eraser's own live lease, the transaction deletes every row and
//!    writes the tombstone, and the recall tier (#18) sweeps the index.
//! 6. The slot becomes [`Slot::Erased`].
//!
//! Why the fence and the close come **before** the erase, where design
//! §6.3 erases first and fences after the commit: both keep the design's
//! point (no lease gap, the lease is never released), and this order also
//! guarantees nothing of the session is running while the erase runs. A
//! flush that committed just before the erase cannot then mirror into the
//! recall index after the erase swept it, and no write-queue worker,
//! intent replay (#11) or image intent (#22) is mid-job. The store's
//! fence (`store::erase::check_fence`) is then a second line, not the only
//! one: a straggler from this handle meets the tombstone and is refused.
//! The recall cache (#14), the access dirty set (#30) and the graph's
//! vectors (#8) are per-`Memory` and go with the handle, which nothing
//! holds once the request ends. The `--ledger` file is not scrubbed (#23
//! user decision).
//!
//! The cost of that order: an erase that fails before its commit (a store
//! error) has already discarded the RAM tail, which the caller was deleting
//! anyway. The lease is then released, holder-scoped, and a pinned session
//! goes back to [`Slot::HeldElsewhere`], so the retry loop serves it again
//! from what is durable.
//!
//! # A session that is not attached
//!
//! Held by another process, failed, erased already, or never attached
//! here: the store erase runs with the CLI's own eraser identity, under the
//! attach lock so a background attach of the same id cannot interleave. A
//! live lease held elsewhere is [`EraseAnswer::HeldElsewhere`] (409), as the
//! CLI's exit 1. A repeat finds the tombstone and reports `already_absent`.
//!
//! # A one-session serve
//!
//! Its fence ends the process (`LeaseLossPolicy::ExitProcess`, JE2E-4), so
//! erasing its only session answers the request and then winds the process
//! down: there is nothing left for it to serve. The erase runs on a task the
//! shutdown waits for (beside the detaches), so the exit cannot cut it short.

use std::sync::Arc;
use std::time::Duration;

use super::{SessionRegistry, Slot, PINNED_RETRY};
use crate::mcp::serve::session::AttachedSession;
use crate::mcp::serve::shutdown::{close_bounded, LEASE_RELEASE_GRACE, SHUTDOWN_GRACE};
use crate::mcp::serve::stages::{ShutdownProgress, Stage};
use crate::store::lease::LeaseHolder;
use crate::store::{EraseOutcome, EraseReport, GraphStore};
use crate::types::{AgentId, SessionId, StoreError};

/// What [`SessionRegistry::erase`] answers. The admin route maps each to its
/// HTTP status.
#[derive(Debug)]
pub(in crate::mcp::serve) enum EraseAnswer {
    /// The store committed (200). `already_absent` on a repeat.
    Erased(EraseReport),
    /// A live writer in another process holds the session (409); nothing
    /// was touched.
    HeldElsewhere { holder: String, age: Duration },
    /// The session is being erased or detached right now, or the serve is
    /// shutting down (503 with `Retry-After`).
    Busy { retry_after: Duration },
    /// The store refused or failed (500). `erased` is `true` when the lease
    /// row reads back as the tombstone, so the session is durably erased and
    /// only a later step (the recall index sweep) failed: a repeat retries it.
    Failed { error: StoreError, erased: bool },
}

impl SessionRegistry {
    /// Erase session `id` from this process (design §6.3). See the module
    /// docs for the order and why. The caller has authorized the request
    /// (`erase` capability, `id` in scope) and checked the confirm.
    ///
    /// Runs on a task of its own that the process shutdown waits for (like
    /// a detach), so neither a client that hangs up nor a shutdown that
    /// starts meanwhile can stop it between its fence and its commit.
    pub(in crate::mcp::serve) async fn erase(self: &Arc<Self>, id: &str) -> EraseAnswer {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let registry = Arc::clone(self);
        let owned = id.to_string();
        let task = tokio::spawn(async move {
            let answer = registry.erase_now(&owned).await;
            let _ = tx.send(answer);
        });
        {
            let mut detaches = self.detaches.lock();
            detaches.retain(|task| !task.is_finished());
            detaches.push(task);
        }
        rx.await.unwrap_or_else(|_| EraseAnswer::Failed {
            error: StoreError::Invariant("the erase task ended without an answer".into()),
            erased: false,
        })
    }

    /// [`SessionRegistry::erase`]'s body, on its task.
    async fn erase_now(self: &Arc<Self>, id: &str) -> EraseAnswer {
        if self.is_closing() {
            return EraseAnswer::Busy {
                retry_after: Duration::from_secs(1),
            };
        }
        // Claim the slot. Everything after this sees `Erasing`.
        let (attached, prior) = {
            let mut slots = self.slots.lock();
            match slots.get(id) {
                Some(Slot::Erasing | Slot::Detaching) => {
                    return EraseAnswer::Busy {
                        retry_after: Duration::from_secs(1),
                    };
                }
                Some(Slot::Live(session)) => {
                    let session = Arc::clone(session);
                    slots.insert(id.to_string(), Slot::Erasing);
                    (Some(session), None)
                }
                _ => (None, slots.insert(id.to_string(), Slot::Erasing)),
            }
        };
        match attached {
            Some(session) => self.erase_attached(id, session).await,
            None => self.erase_unattached(id, prior).await,
        }
    }

    /// Steps 2 to 6 of the module docs for a live session.
    async fn erase_attached(
        self: &Arc<Self>,
        id: &str,
        session: Arc<AttachedSession>,
    ) -> EraseAnswer {
        tracing::info!(session = %id, "lambo serve: erasing an attached session");
        // Step 2, then 3: no watcher left to read the fence as a lost lease.
        session.tasks.stop();
        session.mem.fence_for_erase();
        // Step 4: the detach's per-session stages, under its own record.
        let progress = ShutdownProgress::for_session(id);
        progress.begin(Stage::TransportDrain);
        tokio::select! {
            drained = tokio::time::timeout(SHUTDOWN_GRACE, session.close_mcp_sessions()) => {
                if drained.is_err() {
                    tracing::warn!(
                        session = %id,
                        grace_secs = SHUTDOWN_GRACE.as_secs(),
                        "lambo serve: MCP sessions did not end within the grace window; erasing \
                         anyway (the handle is fenced, so they can no longer read or write)"
                    );
                }
            }
            () = self.closed() => {}
        }
        progress.end(Stage::TransportDrain);
        progress.begin(Stage::SessionClose);
        // A fenced close always errs (it refuses to flush): that refusal
        // is the point here, and `Memory::close` has logged it.
        let _ = close_bounded(&session.mem, &self.early).await;
        progress.end(Stage::SessionClose);
        progress.run(Stage::EventPumpAbort, || session.tasks.event_pump.abort());
        progress.run(Stage::BackgroundTasks, || session.tasks.stop());
        progress.begin(Stage::EndpointRelease);
        session.release_endpoint().await;
        progress.end(Stage::EndpointRelease);
        progress.complete();

        // Step 5, as the holder whose lease the store still shows.
        let store = Arc::clone(session.mem.store());
        let holder = session.mem.lease_holder().clone();
        let previous = Arc::downgrade(&session.mem);
        drop(session);
        let sid = SessionId::new(id);
        let outcome = store.erase_session(&sid, &holder).await;
        match outcome {
            Ok(EraseOutcome::Erased(report)) => {
                self.finish_erased(id, &report);
                EraseAnswer::Erased(report)
            }
            Ok(EraseOutcome::Held { current, age }) => {
                // The lease lapsed while the close ran and another writer
                // took it: that writer owns the session now. Not erased.
                tracing::warn!(
                    session = %id,
                    holder = %current.holder,
                    "lambo serve: the erase found the session held by another writer after this \
                     process closed it; nothing was erased"
                );
                self.after_failed_attached(id, previous);
                EraseAnswer::HeldElsewhere {
                    holder: current.holder,
                    age,
                }
            }
            Err(error) => {
                if is_tombstoned(store.as_ref(), &sid).await {
                    self.slots.lock().insert(id.to_string(), Slot::Erased);
                    tracing::error!(
                        session = %id,
                        error = %error,
                        "lambo serve: the session was erased from the durable store, but a later \
                         step failed; repeat the erase to finish it"
                    );
                    return EraseAnswer::Failed {
                        error,
                        erased: true,
                    };
                }
                tracing::error!(
                    session = %id,
                    error = %error,
                    "lambo serve: erasing an attached session failed before the store committed; \
                     nothing was erased, and its in-memory tail was discarded by the fence"
                );
                // Hand the lease back so the session can be served again
                // from what is durable (holder-scoped: a no-op if it lapsed).
                let released =
                    tokio::time::timeout(LEASE_RELEASE_GRACE, store.release_lease(&sid, &holder))
                        .await;
                if !matches!(released, Ok(Ok(()))) {
                    tracing::warn!(
                        session = %id,
                        "lambo serve: could not release the lease after a failed erase; it will \
                         lapse at TTL"
                    );
                }
                self.after_failed_attached(id, previous);
                EraseAnswer::Failed {
                    error,
                    erased: false,
                }
            }
        }
    }

    /// The erase of a session this process does not hold, as the CLI does
    /// it. `prior` is the slot it had, put back when nothing was erased.
    async fn erase_unattached(self: &Arc<Self>, id: &str, prior: Option<Slot>) -> EraseAnswer {
        let Some(store) = self.shared_store() else {
            self.restore(id, prior);
            return EraseAnswer::Failed {
                error: StoreError::Capability("this serve has no store to erase from".into()),
                erased: false,
            };
        };
        let sid = SessionId::new(id);
        let eraser =
            LeaseHolder::for_this_process(&AgentId::new(crate::cli::erase_session::ERASER_AGENT));
        // A background attach of this id (the pinned retry) cannot run in
        // between: it takes the same lock, and finds the slot `Erasing`.
        let outcome = {
            let _no_attach = self.attach_permits.acquire_many(self.permit_count).await;
            store.erase_session(&sid, &eraser).await
        };
        match outcome {
            Ok(EraseOutcome::Erased(report)) => {
                self.finish_erased(id, &report);
                EraseAnswer::Erased(report)
            }
            Ok(EraseOutcome::Held { current, age }) => {
                self.restore(id, prior);
                EraseAnswer::HeldElsewhere {
                    holder: current.holder,
                    age,
                }
            }
            Err(error) => {
                let erased = is_tombstoned(store.as_ref(), &sid).await;
                if erased {
                    self.mark_erased(id);
                } else {
                    self.restore(id, prior);
                }
                tracing::error!(
                    session = %id,
                    error = %error,
                    erased,
                    "lambo serve: erasing a session failed"
                );
                EraseAnswer::Failed { error, erased }
            }
        }
    }

    /// The store committed: the slot becomes `Erased` (hosted sessions) or
    /// goes (any other id, so the map stays bounded by the hosted set).
    fn finish_erased(&self, id: &str, report: &EraseReport) {
        self.mark_erased(id);
        tracing::info!(
            session = %id,
            already_absent = report.already_absent,
            rows = report.removed.rows(),
            fence_token = report.fence_token,
            "lambo serve: session erased"
        );
    }

    /// Record that `id` is erased: [`Slot::Erased`] for a hosted session,
    /// no slot for any other id. The store's tombstone is what refuses a
    /// later attach either way.
    fn mark_erased(&self, id: &str) {
        let mut slots = self.slots.lock();
        if self.order.iter().any(|hosted| hosted == id) {
            slots.insert(id.to_string(), Slot::Erased);
        } else {
            slots.remove(id);
        }
    }

    /// An attached session whose erase did not commit: a hosted session is
    /// retried in the background like any detached one (it waits for the
    /// old handle to go first); any other id loses its slot.
    fn after_failed_attached(&self, id: &str, previous: std::sync::Weak<crate::memory::Memory>) {
        let mut slots = self.slots.lock();
        if self.order.iter().any(|hosted| hosted == id) {
            slots.insert(
                id.to_string(),
                Slot::HeldElsewhere {
                    retry_at: std::time::Instant::now() + PINNED_RETRY,
                    previous: Some(super::PreviousHandle {
                        mem: previous,
                        detached_at: std::time::Instant::now(),
                    }),
                    warned: false,
                },
            );
        } else {
            slots.remove(id);
        }
    }

    /// Put back the slot an erase that changed nothing replaced.
    fn restore(&self, id: &str, prior: Option<Slot>) {
        let mut slots = self.slots.lock();
        match prior {
            Some(slot) => {
                slots.insert(id.to_string(), slot);
            }
            None => {
                slots.remove(id);
            }
        }
    }

    /// Mark pinned session `id` erased at startup: its attach met the
    /// tombstone (#32 PR 4 note for PR 7). The other sessions are served.
    pub(in crate::mcp::serve) fn mark_erased_at_start(&self, id: &str) {
        tracing::warn!(
            session = %id,
            "lambo serve: a pinned session was erased; serving the other sessions (requests for \
             it get the erased refusal)"
        );
        self.slots.lock().insert(id.to_string(), Slot::Erased);
    }

    /// Whether `id`'s lease row is the erasure tombstone: the typed check an
    /// attach error is classified by (#23: never the message). `false` when
    /// there is no store to ask or the read fails.
    pub(in crate::mcp::serve) async fn is_tombstoned(&self, id: &str) -> bool {
        match self.shared_store() {
            Some(store) => is_tombstoned(store.as_ref(), &SessionId::new(id)).await,
            None => false,
        }
    }

    /// The store every session shares: the template builder's, else the
    /// one the first admitted session brought.
    fn shared_store(&self) -> Option<Arc<dyn GraphStore>> {
        self.attacher
            .as_ref()
            .and_then(|attacher| attacher.template.shared_store())
            .or_else(|| self.store.get().cloned())
    }
}

/// Whether `session`'s lease row reads back as the #23 tombstone.
async fn is_tombstoned(store: &dyn GraphStore, session: &SessionId) -> bool {
    matches!(
        store.read_lease(session).await,
        Ok(Some(row)) if crate::store::erase::is_tombstone(&row)
    )
}
