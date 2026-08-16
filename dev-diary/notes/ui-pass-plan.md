# UI pass: deferred, and what happens when the backend lands

Written as a handoff to whoever picks the portal back up, which will probably be
me with none of this in my head. There is no Lambo instance holding my working
memory, which is a joke I am obliged to notice.

**The UI pass is parked until T1 to T14 in `remediation-tasks.md` are done.**
The portal's remaining problems are not rendering problems. Every one of them is
waiting on data the backend does not currently expose.

---

## Where the portal is now

Landed and deployed:

- Header facts labelled in words. `store: cockroach` / `embedder: bge_m3 / 1024d`
  became "Memory stored in: CockroachDB" and "Meaning model: BGE-M3, 1024
  dimensions". Same values, readable cold.
- A live strip directly under the header. Connection state was previously a
  footer line, which is the least prominent place on the page for the first
  thing a reader needs.
- Stat tiles that say what they count. "nodes" and "edges" are internal
  vocabulary; the tiles now read "things remembered", "records", "connections",
  "load-bearing", "status changes", each with a line of explanation. Writer
  diagnostics moved out of the tiles into a note, because they had the same
  visual weight as the numbers the page is about.
- A trust ladder showing the population of Candidate, Venerable and Canonical
  with what earning each one means, replayed client-side from the transition
  feed because the API publishes transitions rather than a status roll-call.
- The audit trail spanning the grid.
- A CSS pass: wider lede, real type hierarchy, and emphasis in the returned
  memory block so the load-bearing warning and canonical entries stop reading as
  undifferentiated grey monospace.

Landed but inert:

- **A structure tree renderer.** It fetches `/api/graph`, builds a nested tree,
  and marks load-bearing nodes. That endpoint does not exist yet, so the panel
  hides itself. It fails to absent, never to a placeholder, because a page
  drawing a tree of infrastructure it cannot see is worse than a page with no
  tree.

Replaced:

- The Ask box. A wide free-text field beside a primary button reads as a chat
  prompt: it promises a conversation and returns a memory dump. It is now a row
  of named components with the text field demoted to "or name another resource".

---

## What each backend task unlocks

### T3 lands (`/api/graph`, `/api/inspect`)

The single biggest change, and the renderer is already written.

- **Wire the tree.** `loadGraph()` in `web/app.js` already handles the payload
  shape specified in T3. When the endpoint answers, the panel appears. The tree
  has been verified against the live database and looks like this:

  ```
  VPC-Enterprise-Prod
  ├── InternetGateway, RouteTable-Public, SG-PublicWeb,
  │   Subnet-Private-1a, Subnet-Private-1b, lambo-cloudops-db-subnets
  ├── SG-Base-VPC
  │   └── RDS-Lambo-Demo-DB
  └── Subnet-Public-1a
      └── EC2-LamboWebExhibit
  ```

  `Lambda-LamboStats-API` is correctly absent, because it runs outside the VPC.
  The tree states the architecture without being told to, which is the argument
  the whole exhibit is making, so give it room rather than tucking it in a
  sidebar.

- **Add a dependents view to the component lookup.** Selecting `SG-Base-VPC`
  should say, in words, that `RDS-Lambo-Demo-DB` depends on it and would be
  affected. That is the sentence the submission is built on and the page has
  never once been able to say it.

- **Make the tree and the lookup the same interaction.** Clicking a node in the
  tree should be the lookup. Two controls doing one job is how the page got
  confusing the first time.

### T11 lands (recall answers dependency questions)

Today `what depends on SG-Base-VPC` returns the security group and its own
ingress rules, and a prose question returns five results all scored `0.18`.

When that is fixed, the free-text field stops being a liability and the page can
invite real questions again. Until then the named components carry the
interaction, because they always return something meaningful.

Also revisit the score display. Showing `(score 2.94)` next to `(score 2.72)`
tells a reader nothing; it is only worth surfacing if the numbers discriminate.

### T12 resolves (the Function URL 403)

Decides whether the page can link a public stats endpoint at all. If it stays
403, do not link it, and make sure §11 describes the Lambda as IAM-invoked.

### T13 resolves (canonization cadence)

If the default cadence changes, the trust ladder starts moving on its own during
a demo rather than sitting still. Worth checking the ladder animates sensibly
when a promotion lands mid-session, since it has only ever been seen static.

---

## The rule that must survive the UI pass

**The page must not claim anything it cannot show.**

This has already been broken once, by me. The intro said Lambo "names the
workloads that would break and counts them", while `/api/recall` did not name
them and the page had no way to. That sentence has been softened to what the
page can currently demonstrate, and it should only be strengthened again when
T3 makes it true.

Concretely: no copy describing dependents until the dependents panel renders,
and no tree until `/api/graph` answers. The current code already enforces the
second one by hiding the panel.

---

## Decisions already made, so they are not relitigated

- **No Mithril, and no framework.** Investigated properly. It needs no build
  step and there is no CSP on this surface, so both objections I raised first
  were wrong. It is still the wrong call: it renders identical DOM, fixes none
  of the page's actual problems, vendors a third-party dependency into the
  shipped binary along with the NOTICE work that implies, and contradicts the
  design property `serve_web.rs` documents. The one genuine argument is that
  tiles and ladder are torn down and rebuilt on every 1.5s poll, which would
  break focus on any interactive element placed inside them. That is ten lines
  of in-place update if it ever bites, not a framework.

- **No screen recording on this machine.** `ffmpeg -f x11grab` returns a frame
  that is entirely black except the cursor, because XWayland's root window
  cannot see KWin's surfaces. The xdg-desktop-portal ScreenCast request times
  out with "Failed to select screen". Playwright records the browser context
  directly and `vhs` renders terminal sessions, and neither touches the
  compositor. Do not try either again.

- **The returned memory block is built with `textContent`, never `innerHTML`.**
  It is agent-authored content, so templating it into markup would be an XSS
  hole with the graph as the payload. Highlighting is done with one element per
  line, classed by what the line is.

---

## Rough order for the pass itself

1. Tree wired to `/api/graph`, given the space it deserves.
2. Tree node click drives the lookup, replacing the separate control.
3. Dependents panel, with the sentence the exhibit exists to say.
4. Copy strengthened back to match, once 1 to 3 are real.
5. Score display revisited, only if T11 made the numbers mean something.
6. Whatever the layout needs once the tree is occupying real estate. Do this
   last, because the tree changes the page's proportions more than anything
   else on the list.
