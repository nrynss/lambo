//! Property tests: the `MemoryStore` session lease against
//! `formal/LamboProofs/Lease.lean`.

use std::collections::HashMap;
use std::time::Duration;

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::store::lease::testkit::{holder, interaction_batch};
use crate::store::lease::{lease_permits_write, RELEASED_HOLDER};
use crate::test_util::props;

/// Writers contending for one session.
const HOLDERS: u32 = 3;

/// One step of a lease history. `Expire` is the lapse of a lease whose
/// heartbeat starved (time passing past `expires_at`); `Write` is a flush by
/// a holder presenting the token it captured at its last acquire.
#[derive(Clone, Copy, Debug)]
enum Op {
    Acquire(u32),
    Refresh(u32),
    Release(u32),
    Expire,
    Write(u32),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..HOLDERS).prop_map(Op::Acquire),
        (0..HOLDERS).prop_map(Op::Refresh),
        (0..HOLDERS).prop_map(Op::Release),
        Just(Op::Expire),
        (0..HOLDERS).prop_map(Op::Write),
    ]
}

/// Lean `Row`, with time reduced to "live or not": every acquire here uses a
/// TTL far longer than a test run, so a row is expired only after `Expire`
/// or a release.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    holder: String,
    token: u64,
    live: bool,
}

/// Lean `acquire` (and `first` for a session with no row).
fn model_acquire(me: &str, r: Option<&Row>) -> Row {
    match r {
        None => Row {
            holder: me.into(),
            token: 1,
            live: true,
        },
        Some(r) if r.live && r.holder != me => r.clone(),
        Some(r) if r.holder == me => Row {
            holder: me.into(),
            token: r.token,
            live: true,
        },
        Some(r) => Row {
            holder: me.into(),
            token: r.token + 1,
            live: true,
        },
    }
}

/// Lean `release`.
fn model_release(me: &str, r: Option<&Row>) -> Option<Row> {
    r.map(|r| {
        if r.holder == me {
            Row {
                holder: RELEASED_HOLDER.into(),
                token: r.token,
                live: false,
            }
        } else {
            r.clone()
        }
    })
}

/// Lean `Permits` (`formal/LamboProofs/Lease.lean`): a session never leased
/// (token `0`) admits any write; otherwise the writer must present a token
/// at least the current one.
fn spec_permits(current: u64, presented: Option<u64>) -> bool {
    current == 0 || presented.is_some_and(|p| current <= p)
}

proptest! {
    #![proptest_config(props::config())]

    /// `Lease.token_mono`, `Lease.stale_token_refused`,
    /// `Lease.later_holder_excludes_earlier` and the three arms of
    /// `Lease.acquire` / `Lease.release` (`formal/LamboProofs/Lease.lean`),
    /// over random histories of acquire, refresh, release, lapse and write by
    /// three holders against `MemoryStore`:
    ///
    /// * after every step the row is the one the Lean model computes;
    /// * `current_token` never decreases;
    /// * a flush lands exactly when Lean `Permits` admits the captured token
    ///   against the model row's token;
    /// * whenever another identity holds the row, every earlier holder's
    ///   captured token fails `lease_permits_write`, and its flush is refused
    ///   as a stale write by the store;
    /// * the current holder's captured token passes, and its flush lands;
    /// * `Lease.Safe`'s released case: after a release and before anyone
    ///   acquires again, the releaser's captured token (still the row's
    ///   token) passes, and an older generation's does not.
    #[test]
    fn lease_tokens_rise_and_fence_out_every_earlier_holder(ops in vec(op(), 1..24)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let store = MemoryStore::new();
            let sid = props::sid();
            let ttl = Duration::from_secs(3600);
            let holders: Vec<_> = (0..HOLDERS).map(|i| holder(&format!("prop-{i}"), i + 1)).collect();
            let names: Vec<String> = holders.iter().map(|h| h.token()).collect();
            let mut model: Option<Row> = None;
            let mut captured: HashMap<usize, u64> = HashMap::new();
            let mut last_token = 0u64;

            for op in ops {
                match op {
                    Op::Acquire(i) | Op::Refresh(i) => {
                        let i = i as usize;
                        let out = if matches!(op, Op::Acquire(_)) {
                            store.acquire_lease(&sid, &holders[i], ttl).await
                        } else {
                            store.refresh_lease(&sid, &holders[i], ttl).await
                        }
                        .expect("acquire");
                        let next = model_acquire(&names[i], model.as_ref());
                        let held = model.as_ref() == Some(&next) && next.holder != names[i];
                        match out {
                            LeaseOutcome::Acquired(info) => {
                                prop_assert!(!held, "acquired a live lease held by another: {:?}", model);
                                captured.insert(i, info.token);
                            }
                            LeaseOutcome::Held { .. } => prop_assert!(held, "refused, model {:?}", model),
                        }
                        model = Some(next);
                    }
                    Op::Release(i) => {
                        store.release_lease(&sid, &holders[i as usize]).await.expect("release");
                        model = model_release(&names[i as usize], model.as_ref());
                    }
                    Op::Expire => {
                        store.force_expire_lease(&sid);
                        if let Some(r) = model.as_mut() {
                            r.live = false;
                        }
                    }
                    Op::Write(i) => {
                        if let Some(&t) = captured.get(&(i as usize)) {
                            let current = model.as_ref().map_or(0, |r| r.token);
                            let res = store.flush(&interaction_batch(&sid, "w"), Some(t)).await;
                            prop_assert_eq!(
                                res.is_ok(),
                                spec_permits(current, Some(t)),
                                "flush disagrees with Lean Permits on the model row: {:?}", res
                            );
                        }
                    }
                }

                let row = store.read_lease(&sid).await.expect("read");
                let row = row.map(|r| (r.holder, r.token));
                prop_assert_eq!(row.clone(), model.as_ref().map(|m| (m.holder.clone(), m.token)), "store row != Lean row");
                let Some((row_holder, token)) = row else { continue };

                // token_mono
                prop_assert!(token >= last_token, "token went down: {} -> {}", last_token, token);
                last_token = token;

                let live_holder = names.iter().position(|n| *n == row_holder);
                prop_assert!(live_holder.is_some() || row_holder == RELEASED_HOLDER);
                for (&a, &t) in &captured {
                    if Some(a) == live_holder {
                        // later_holder_excludes_earlier, second half
                        prop_assert!(lease_permits_write(token, Some(t)), "holder {} refused", a);
                        prop_assert!(store.flush(&interaction_batch(&sid, "holder"), Some(t)).await.is_ok());
                    } else if live_holder.is_some() {
                        // stale_token_refused / later_holder_excludes_earlier
                        prop_assert!(!lease_permits_write(token, Some(t)), "stale {} passes", a);
                        let res = store.flush(&interaction_batch(&sid, "stale"), Some(t)).await;
                        prop_assert!(matches!(res, Err(StoreError::StaleWrite(_))), "stale flush: {:?}", res);
                    } else {
                        // Safe, released case: the row's generation is still
                        // the releaser's, so its token passes; any older one
                        // is refused.
                        prop_assert!(t <= token, "captured {} above the row's {}", t, token);
                        let res = store.flush(&interaction_batch(&sid, "released"), Some(t)).await;
                        if t == token {
                            prop_assert!(lease_permits_write(token, Some(t)), "releaser {} refused", a);
                            prop_assert!(res.is_ok(), "releaser flush: {:?}", res);
                        } else {
                            prop_assert!(!lease_permits_write(token, Some(t)), "older {} passes", a);
                            prop_assert!(matches!(res, Err(StoreError::StaleWrite(_))), "older flush: {:?}", res);
                        }
                    }
                }
            }
            Ok::<(), TestCaseError>(())
        })?;
    }
}
