# Deployment and submission

The path from a remediated tree to a thing judges can look at. Code fixes live
in `remediation-tasks.md`; nothing here changes behaviour.

**Tasks are D1 to D3.** They are numbered separately from the remediation tasks
(T1 to T12) because the two lists run on different clocks and cross-reference
each other. Where a D task waits on a T task, it says so.

---

## Done

### Ship `drive_mcp_soak.py` as a labelled demo artifact

Moved to `examples/drive_mcp_soak.py`, with `examples/README.md` and a header on
the script.

It produced the Canonical status the exhibit displays, so deleting it quietly
would have made that status unreproducible. The label is specific: the
structural gates were earned (blast radius above 5 over structural edges,
`gc_survived` at 3, top-decile score against 20 or more peers, coverage above
0.3, all cleared by `VPC-Enterprise-Prod` at a real blast radius of 7), and
Stage 2's requirement of three distinct origin interactions was satisfied by
replaying the same derives. Each replay is a distinct interaction to the engine.
None of them is distinct work.

Anyone citing the exhibit's canonization should cite the blast radius and the
audit trail, and know where the interaction count came from.

---

## D1 — Clean redeploy from scratch

**Blocked by:** remediation **T6** (launcher fixes)
**Blocks:** D2, D3

The running instance was repaired by hand while the launcher was being fixed:
packages installed over SSH, the bootstrap patched in place and re-run. It works,
and it is **not** a clean product of the current script. Until a launch from zero
succeeds, "rebuildable from the scripts alone" is a claim that cannot be made,
and that claim is load-bearing for the submission.

Sequence:

1. `teardown.py --confirm` on the exhibit instance and its Elastic IP. Keep the
   network, RDS, Lambda and the secret; nothing about those is in question.
2. `launch_exhibit_ec2.py` from clean, with no manual steps afterwards.
3. Re-point the A record if the Elastic IP changed. It should not, since the
   address is allocated separately and re-associated, but check rather than
   assume.
4. Wait for Caddy's certificate, confirm `/healthz`, `/api/stats` and
   `/api/events` answer, and confirm the session still shows `canonical: 1` with
   the full status walk.

Notes:

- The binary the instance runs is built natively by the release workflow on
  the Ubuntu 24.04 runners (glibc 2.39); the instance's Ubuntu 26.04 (glibc
  2.41) is newer than the build environment, so the shipped binary runs. Use
  the release artifact; build, `scp`, restart: about four minutes.
- The exhibit runs x86_64 on Ubuntu 26.04 and that is the shipped path. The
  launcher's arm64 branch is not exercised and is not being validated.
- Do not redeploy while a capture is running. D2 depends on the service staying
  up for the length of a take.

---

## D2 — Recording and video

**Blocked by:** remediation **T8** (`03_crossover_protect.py` has never run), D1

Nothing gets recorded until the climax script has executed successfully at least
once and the exhibit is the clean article.

Tooling, already built and working:

- **Portal footage**: `scripts/recording/capture-portal.mjs`. Playwright records
  the browser context directly, so nothing depends on the compositor and a
  re-run produces the same footage. It drives the page as a judge would, and
  waits on the Ask button rather than racing it, because recall takes about 4.5
  seconds on the exhibit against 750ms locally.
- **Terminal footage**: `vhs`, which renders a scripted session to video with no
  compositor involved. Needs `pacman -S vhs ttyd`.

Screen capture on this machine does not work and fails quietly, which is why
neither of the above uses it. `ffmpeg -f x11grab` returns a frame that is
entirely black except the mouse cursor, because XWayland's root window cannot
see KWin's surfaces. The xdg-desktop-portal ScreenCast request times out with
"Failed to select screen". Do not spend time on either again.

The video has to carry the argument, not the inventory: agents provisioned real
AWS infrastructure, one of them tried to delete a security group another
agent's database sits behind, and Lambo stopped it. The service list is
supporting detail.

---

## D3 — Docs and submission text

**Blocked by:** D1, and anything in remediation **T6** that changes the stack

Every one of these is currently contradicted by what is running:

- `README.md` says `AWS services used: None yet`. Six services are live.
- `site/src/content/docs/hackathon.mdx` carries three `Not yet` rows: demo URL,
  video, AWS services. The demo URL exists.
- `docs/plans/multi-agent-cloudops-aws-plan.md` §11 describes a `t4g.micro` on
  Graviton and a public Lambda Function URL. The exhibit is an `m7i-flex.large`
  on x86_64 running Ubuntu 26.04, and the Function URL returns 403 with a
  correct resource policy and no Organization to explain it.

Two things to get right rather than fast:

- **The Function URL.** The Lambda works: invoked directly it returns live
  counts read from CockroachDB through the scoped secret. The public URL
  previously 403d because (post-Oct-2025) a public function URL requires BOTH
  `lambda:InvokeFunctionUrl` AND `lambda:InvokeFunction` in its resource
  policy; the missing second statement was added (remediation T10) and the URL
  now returns HTTP 200. §11 should describe it as the live public endpoint at
  the URL shown on deploy, per the T10 resolution.
- **The portal's own copy.** It currently promises that Lambo "names the
  workloads that would break". `/api/recall` does not do that; remediation T3
  makes it true. If T3 does not land, that sentence comes down before the URL is
  submitted. This is a hard cutover, not something to discover late.

Draft freely, land last. Anything written before D1 gets rewritten.

---

## Order

```
T6 ──► D1 ──► D2      redeploy, then capture
T8 ──────────► D2      the climax script must have run
       D1 ──► D3      docs land after the stack stops moving
```

Critical path to a submission: **T6 → D1 → D2**, with **T8** joining before D2,
and **D3** trailing D1.
