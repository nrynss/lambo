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

## Not done

- A boot-id match does not override the `unknown-host` exclusion. Two
  processes that both fell back to the placeholder host name but share a
  boot are on one machine, but that case is rare and the hedge is safe.
- Consequence: another process can no longer release a holder's row by
  rebuilding a `LeaseHolder` from `agent@host#pid`, since it cannot
  reproduce the holder's boot and namespace fields. Nothing in the product
  did that; `tests/serve_proxy_multi_client.rs`'s `release_row` did, and now
  runs the documented `OPERATOR_OVERRIDE` statement instead (same resulting
  row). Noted in the CHANGELOG for library callers.
