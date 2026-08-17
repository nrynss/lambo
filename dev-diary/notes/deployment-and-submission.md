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

**CORRECTION 2026-08-18: `teardown.py` cannot do step 1 as written.** It has no
target selection (only `--confirm`, `--verify-only`, `--force-delete-secret`),
and a dry run lists **18 resources**: the Lambda, both IAM roles, the RDS
instance, the DB subnet group, `SG-PublicWeb`, `SG-Base-VPC`, the route table,
internet gateway, all three subnets, the VPC and the DSN secret, alongside the
instance and the EIP. Running it would destroy the very resources the demo
narrates, and `launch_exhibit_ec2.py` rebuilds only the exhibit host, so
recovery would need `provision_network.py` + `provision_app_data.py` and would
mint **new** resource ids that the recorded graph does not know (it stores
`SG-Base-VPC = sg-071b52ffe5950efdf`). D1 therefore terminates the instance
only.

Sequence:

1. Terminate **only** the exhibit instance, leaving the Elastic IP allocated and
   the network, RDS, Lambda and secret untouched. Do not run
   `teardown.py --confirm`.
2. `launch_exhibit_ec2.py` from clean, with no manual steps afterwards.
3. Re-point the A record if the Elastic IP changed. It should not, since the
   address is allocated separately and re-associated, but check rather than
   assume.
4. Wait for Caddy's certificate, confirm `/healthz`, `/api/stats` and
   `/api/events` answer, and confirm the session still shows `canonical: 1` with
   the full status walk.

Notes:

- **There is no binary step any more.** v0.2.0 shipped, so the launcher's own
  user data does the install: it fetches `lambo-0.2.0-<arch>` from the GitHub
  release and verifies it against the published `.sha256` before installing.
  `DEFAULT_LAMBO_VERSION` in `launch_exhibit_ec2.py` is already `0.2.0`, and
  the asset arch is chosen from the instance type, so an `m7i-flex.large`
  pulls `linux-x86_64` without being told. Nothing to build, nothing to `scp`,
  nothing to restart by hand, which is exactly the "clean product of the
  current script" claim D1 exists to establish. The old four-minute
  build-and-copy loop is gone; drop it from the sequence rather than doing it
  out of habit.
- The binary is built natively by the release workflow on the Ubuntu 24.04
  runners (glibc 2.39). The instance's Ubuntu 26.04 carries glibc 2.41, newer
  than the build environment, so the shipped binary runs. That direction is the
  one that works; the reverse is what T12 was about.
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

## ✅ D3 done: docs and submission text (2026-08-17)

Landed ahead of D1 rather than after it, on the reasoning that the stack it
describes is not going to move: D1 rebuilds the same stack from the same
script, so the instance id changes and nothing in the copy does. The one thing
that would have forced a rewrite, the Elastic IP and therefore the hostname,
is allocated separately and re-associated, so `lambo.nryn.dev` survives the
redeploy. If D1 somehow lands on a different address, the only edit needed is
the A record, not these pages.

Every claim below was checked against the running exhibit before it was
written, not against the plan.

**Verified live first:**

- `https://lambo.nryn.dev/` → 200, `/healthz` → `ok`, `/api/stats` → session
  `cloudops-exhibit`, 113 nodes / 485 edges / 41 concepts / 1 canonical / 7
  canonization events, `mode: reader`.
- The Lambda Function URL → 200, same session, with
  `VPC-Enterprise-Prod` reported Canonical at blast radius 7.

**Changed:**

- `README.md`: the `None yet` section is now a six-service table that leads
  with the outage that did not happen and keeps the table as supporting detail,
  per §11's instruction. The `v0.1` scope line is now `v0.2`.
- `site/src/content/docs/hackathon.mdx`: demo URL and AWS services flip to
  **Met** with the two live URLs; a new "AWS, and what is actually running"
  section carries the argument and the table; the `No demo URL` and
  `No AWS services` paragraphs are gone.
- `docs/plans/multi-agent-cloudops-aws-plan.md` §11: the EC2 row now records
  what was actually built (`m7i-flex.large`, x86_64, Ubuntu 26.04) against what
  the plan first assumed, and the Lambda row records the URL as live with the
  T10 root cause named.
- `site/src/content/docs/{demo,cli}.mdx`: `only one in v0.1` → `v0.2`.

**Deliberately left saying `Not yet`:** the video row. D2 has not happened, and
a submission page that claimed otherwise would break the exact habit the page's
closing note is about.

**Checked, no change needed:** the portal's own copy. The sentence this note
worried about, Lambo "names the workloads that would break", is not in
`web/index.html`. What is there is "marks which memories other work depends on,
so an agent knows what is dangerous to change", which T3 makes literally true.
The hard cutover this note was holding open never became necessary.

One honest boundary is stated in both README and hackathon page rather than
blurred: AWS runs *around* Lambo, not inside it, because the released binary
still calls no AWS API. Bedrock would be the entry that changes that, and it
stays out of the table while the model-access request is unapproved.

---

## Order

```
T6 ──► D1 ──► D2      redeploy, then capture
T8 ──────────► D2      the climax script must have run
       D3            done; landed early, see the note in that section
```

Critical path to a submission is now just **D1 → D2**, with **T8** joining
before D2. T6 and T12 are done, and D3 is done. The only row on the hackathon
page still reading `Not yet` is the video, and D2 is what clears it.
