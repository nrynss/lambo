# #126: lease holder names its boot and PID namespace

The lease holder token was `agent@host#pid`. Host plus pid does not identify
a process: containers on the host network share the host name but not the
pids, two hosts that both fall back to `unknown-host` match, and a pid is
reused after a reboot. So the #118 "looks gone" hint had to stay hedged.

## Decision

- `LeaseHolder::token` appends `;boot=<id>;pidns=<inode>` (each only when
  the OS reports it) **only when the holder names this process** (pid and
  host equal this process's). A process can read only its own boot id and
  PID namespace, so a holder describing another pid carries neither. This
  keeps `LeaseHolder`'s fields unchanged: no library API break, and the
  test holders built with fake pids keep the old format.
- Sources: Linux `/proc/sys/kernel/random/boot_id` and the inode of
  `/proc/self/ns/pid`; macOS `kern.bootsessionuuid` through
  `libc::sysctlbyname` (libc was already a dependency); nothing elsewhere.
  Read once and cached. A boot id with characters other than ASCII
  alphanumerics and `-` is dropped, so the suffix can never contain `@`,
  `#`, `;` or `=`.
- `store::lease::parse_holder` replaces `holder_host_and_pid`. It splits
  from the right as before, then reads `;key=value` fields after the pid,
  ignoring unknown keys. An old token parses with no instance.
- The refusal says "the holder process is gone" only when the boot ids are
  both present and equal and the namespace ids are both present and equal
  (both absent counts only off Linux, which has no PID namespaces), and
  `kill(pid, 0)` answers `ESRCH`. Everything else keeps the 0.5.0 hedged
  text. The existing gates (same host, not the fallback host, not this pid)
  are unchanged.
- Lease acquire, refresh, release, fencing and takeover are untouched. A
  process's token is stable for its life, so refresh and release still match.
- The proxy's `holder_is_on_host` already splits host from the last `@` and
  `#`, so it reads the same host from either format; a test pins that.

## Review follow-ups

- L9: when boot and namespace both match, the host-name gate is skipped
  (including the `unknown-host` placeholder). Same boot plus same PID
  namespace is stronger evidence of a shared pid space than a host name; a
  container with its own UTS name on the host PID namespace is the case it
  covers. Without the match the 0.5.0 host gates apply unchanged.
- L8: the plain wording says "this boot (and PID namespace, where the
  platform has them)", since macOS has none.

## Residual risk (review L1)

A VM or microVM cloned from a snapshot, or a process restored with CRIU,
keeps the host name and the boot id, and every Linux kernel gives its root
PID namespace the same inode (4026531836). Two clones sharing one store can
therefore match on both, and a live holder on the other clone is called
gone. The lease, fencing and takeover are unaffected (the hint is advisory),
and cli.mdx tells operators on cloned fleets to keep confirming by hand.
Closing this would need a per-boot nonce the clone does not inherit, which
no kernel interface provides portably.
- Consequence: another process can no longer release a holder's row by
  rebuilding a `LeaseHolder` from `agent@host#pid`, since it cannot
  reproduce the holder's boot and namespace fields. Nothing in the product
  did that; `tests/serve_proxy_multi_client.rs`'s `release_row` did. It now
  runs the override scoped with `AND holder = ?` and asserts one row
  changed, so the holder check is inside the statement (review M2: the
  unscoped override, or a read then an override, can evict a writer that
  took the lease in between). The CHANGELOG lists both the format change
  and this under Breaking (library API) and recommends the holder-scoped
  statement. A `release_lease_matching(session, token)` store method was
  not added: it would be a trait method in every adapter for a case the
  product never hits, and the scoped SQL already serves operators.
