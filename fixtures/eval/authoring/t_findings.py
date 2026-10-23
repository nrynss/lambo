from common import C, NA
DF = 'dev-diary/lambo-for-mooshik/DOGFOOD-FINDINGS.md'
T = 'dogfood-findings'
C('fnd-01', 'Observation',
  "Across the five J0 agent runs, before the protocol block existed in prompts, zero lambo calls were made, although AGENTS.md obliged it and every agent had read AGENTS.md: compliance came from the prompt, not the agent's judgement or the tool's affordances.",
  DF, topic=T,
  q=["Did agents use lambo when only AGENTS.md told them to?",
     ("Where did lambo-protocol compliance come from in the early dogfood runs?", 'rationale', {'top-05': 1, 'top-04': 1})],
  para="In the five J0 agent runs, which predate the protocol block in prompts, agents made no lambo calls at all, even though AGENTS.md required it and all of them had read it. Compliance came from the prompt, not from agent judgement or tool affordances; the obligation has to travel in the prompt.",
  rbd=[("Across the five J0 agent runs, agents called lambo_recall at session start because AGENTS.md obliged it, so prompt text was unnecessary.", 'polarity', 'contradicts'),
       ("Claude Code skips the lambo-dogfood protocol even when it is in the prompt, unlike other harnesses, so it needs hooks.", 'exception', 'distinct_entity')],
  facets=["zero lambo calls", "compliance came from the prompt"])
C('fnd-02', 'Observation',
  "dedup_rate.py over the early ledger window: 7 write calls, 24 concepts created, 2 matched, a dedup rate of 0.077; j1-reviewer-r1 was the only agent to match anything (0.333).",
  DF, topic=T,
  q=["What dedup rate did the early dogfood ledger show?",
     ("dedup_rate.py", 'exact-identifier')],
  para="Early in dogfooding, dedup_rate.py counted 7 write calls producing 24 created concepts and 2 matches, a 0.077 dedup rate, and j1-reviewer-r1 was the one agent that matched at all, at 0.333.",
  rbd=[("dedup_rate.py over the early ledger window: 24 write calls, 7 concepts created, a dedup rate of 0.333 for the orchestrator.", 'quantity', 'contradicts')],
  facets=["0.077", "j1-reviewer-r1 0.333"])
C('fnd-03', 'Logic',
  "The recall-twice protocol: recall the topic before reading code, then a targeted recall on the real names afterwards. It came from the J0 arithmetic-deviation reasoning (accepted − written − write_failed), which was in the graph all along but only findable once the reader knew the key name.",
  DF, topic=T,
  q=["What is the recall-twice protocol and why does it exist?",
     ("Why was the J0 arithmetic-deviation concept hard to find?", 'vocabulary-mismatch')],
  para="Agents now recall twice: once on the topic before opening code, then again with the actual identifiers they found. The rule exists because the J0 accepted − written − write_failed reasoning sat in the graph the whole time yet a topic-level query ranked it too low until the reader knew the key name.",
  rbd=[("The recall-first protocol: one lambo_recall before any edit, enforced in Claude Code by a PreToolUse hook bounded at two denials.", 'topic-neighbour')],
  facets=["topic recall before code", "targeted recall on real names after"])
C('fnd-04', 'Observation',
  "Before the J1 re-pin, every write from every subagent was attributed to claude-orchestrator, so from the graph's side there had only ever been one agent and independent multi-agent convergence was impossible; the swarm-policy clock had not started.",
  DF, topic=T,
  q=["Why could the swarm canonization policy not be tested before the J1 re-pin?",
     ("Which agent were all subagent writes attributed to before J1?", 'fact-lookup', {'srv-02': 1})],
  para="Until the rig re-pinned to a J1 binary, all subagent writes were stamped claude-orchestrator, so the graph only ever saw one agent; independent convergence across agents could not happen at all, and the fair test of the swarm policy had not begun.",
  rbd=[("Before the J1 re-pin, each subagent's writes carried its own agent id, so convergence was measurable but slow.", 'polarity', 'contradicts')],
  facets=["all attributed to claude-orchestrator", "convergence impossible"])
C('fnd-05', 'Logic',
  "The 2026-08-27 fair test is precision and recall against ground truth named before the fact: do the four canon-shaped invariants promote while the other ~96 concepts do not. Either direction failing is a finding.",
  DF, topic=T,
  q=["What was the 2026-08-27 canonization fair test?",
     ("Why can the fair test not just ask whether anything promotes?", 'rationale', {'fnd-06': 1})],
  para="For 2026-08-27 the test was fixed in advance as precision and recall against named ground truth: the four canon-shaped invariants should promote and the remaining ~96 concepts should not; failure in either direction counts as a finding.",
  rbd=[("The 2026-08-27 fair test passes if any concept promotes within a week of distinct per-call ids.", 'correction', 'supersedes')],
  facets=["four invariants promote", "~96 do not", "either direction failing is a finding"])
C('fnd-06', 'Constraint',
  "Ground truth for the fair test, the four canon-shaped invariants: (1) the lease stays, a proxy moves the call not the write; (2) the wedge invariant, never acquire a lease you cannot serve; (3) the cooperative-identity ruling; (4) the async-ack rule, a write may be acked asynchronously only when its result does not gate the caller's next action.",
  DF, topic=T,
  q=["What are the four canon-shaped invariants in the dogfood store?",
     ("What is the wedge invariant?", 'fact-lookup'),
     ("When may a write be acknowledged asynchronously?", 'fact-lookup', {'ack-04': 1})],
  para="The fair test's ground truth is four invariants: a proxy moves the call, never the write, so the lease stays; never acquire a lease you cannot serve (the wedge invariant); the cooperative-identity ruling; and async acks only for writes whose result does not gate the caller's next step.",
  rbd=[("The async-ack rule: every write is acknowledged asynchronously, including those whose result gates the caller's next action.", 'exception', 'contradicts')],
  facets=["proxy moves the call not the write", "never acquire a lease you cannot serve", "async ack only if the result does not gate the next action"])
C('fnd-07', 'Logic',
  "Blast radius cannot fire on concepts nobody linked: the orchestrator's derives are mostly free-floating prose with almost no parent_of pairs, so 'nothing is load-bearing' is partly a statement about how agents write; nothing on the write surface nudges a caller to declare what a constraint supports.",
  DF, topic=T,
  q=["Why did the blast-radius warning never fire on the dogfood store?",
     ("Do lambo's write tools encourage declaring what a constraint supports?", 'contrast', {'g3-01': 1})],
  rbd=[("Canonization had never completed on the dogfood store: 0 of 386 concepts carried a stored blast_radius after thousands of daemon cycles.", 'topic-neighbour')],
  facets=["concepts nobody linked", "almost no parent_of pairs", "write surface favours islands"])
C('fnd-08', 'Observation',
  "Two-product probe (2026-08-20, J2-L1): cursor-agent scrubs TMPDIR from its MCP child while opencode passes macOS's per-user TMPDIR through, so the same binary, store and session derived two endpoint directories and cross-client memory was silently absent on default wiring.",
  DF, topic=T,
  q=["Why did cursor-agent and opencode end up with different lambo endpoints?",
     ("TMPDIR", 'exact-identifier'),
     ("What did the two-product live probe find about endpoint derivation?", 'vocabulary-mismatch')],
  para="In the 2026-08-20 two-product probe (J2-L1), cursor-agent removed TMPDIR from its MCP child's environment and opencode kept macOS's per-user TMPDIR, so one binary on one store and session computed two endpoint directories, and memory across the two clients was silently missing with default wiring.",
  rbd=[("Two-product probe: with TMPDIR aligned, everything J2 promises worked across products, including cross-product read-your-writes in both directions.", 'exception', 'distinct_entity'),
       ("Two-product probe: cursor-agent passes macOS's per-user TMPDIR through while opencode scrubs it.", 'polarity', 'contradicts')],
  facets=["cursor-agent scrubs TMPDIR", "opencode passes it", "two endpoint directories"])
C('fnd-09', 'Constraint',
  "Server-side wait budgets must fit under client spawn gates (J2-L2): serve's 50 s lease-lapse wait exceeded opencode's ~32 s MCP startup tolerance, turning a recoverable wait into 'server unavailable, no tools'.",
  DF, topic=T,
  q=["Why did opencode report lambo as unavailable while the serve was waiting for the lease?",
     ("How long does opencode wait for an MCP server to start?", 'fact-lookup')],
  para="J2-L2: a server's waits have to finish inside the client's startup window. The serve waited 50 s for the lease to lapse, but opencode tolerates only about 32 s at MCP startup, so a wait that would have recovered became 'server unavailable, no tools'.",
  rbd=[("J2-R1-1 reproduced against real products: with the holder killed mid-call, the client hung 121.7 s, exactly its configured timeout.", 'topic-neighbour')],
  facets=["50 s lease-lapse wait", "~32 s startup tolerance"])
C('fnd-10', 'Observation',
  "J2-R1-1 reproduced against real products: with the holder killed mid-call, the proxy saw the closed connection in 18 ms but sent the client nothing, and the client hung 121.7 s, exactly its configured timeout (MCP error -32001).",
  DF, topic=T,
  q=["What happened to a client when the lease holder was killed mid-call before the J2 fix?",
     ("MCP error -32001", 'exact-identifier')],
  rbd=[("J2-R1-1: with the holder killed mid-call, the proxy returned an immediate honest error carrying the in-flight id within 18 ms.", 'polarity', 'contradicts')],
  facets=["detected in 18 ms", "client hung 121.7 s"])
C('fnd-11', 'Constraint',
  "cursor-agent needs --trust and --force for lambo's MCP tools, otherwise every MCP tool call is silently rejected; macOS also has no timeout(1).",
  DF, topic=T,
  q=["Why are cursor-agent's lambo tool calls silently rejected?"],
  rbd=[("opencode needs --trust and --force for MCP tools; cursor-agent accepts them by default.", 'repository', 'contradicts')],
  facets=["--trust and --force"])
NA(T, "What did the 2026-08-27 fair test find?", note="the outcome is not in this corpus")
NA(T, "Which client products other than cursor-agent and opencode were probed against J2?")
NA(T, "How many dogfood sessions ran between 2026-08-20 and 2026-08-27?")
NA(T, "What dedup rate did the dogfood ledger show in September?")

T = 'g3-graph'
G3 = DF
C('g3-01', 'Observation',
  "G3 spike (2026-08-21): the dogfood graph had 386 concepts in 41 components; the giant component held 92% of Resources and 85% of Entities but only 37% of Logic and 30% of Observation concepts, because lambo_derive writes only intra-call CoOccurrence cliques plus the rarely used parent_of, while record_action links artifacts to shared file and commit hubs.",
  G3, topic=T,
  q=["Why do derived decisions end up disconnected in the lambo graph?",
     ("How many components did the dogfood graph have in the G3 spike?", 'fact-lookup'),
     ("Is the islands problem a graph-algorithm problem?", 'contrast')],
  para="The 2026-08-21 G3 spike found 386 concepts split into 41 components. Most Resources (92%) and Entities (85%) sat in the giant component, but only 37% of Logic and 30% of Observation concepts did: record_action ties artifacts to shared file and commit hubs, while lambo_derive only writes CoOccurrence cliques within a call and the seldom-used parent_of, so reasoning strands.",
  rbd=[("Adding the Derives co-parent projection to the G3 graph collapses its 41 islands to only 40.", 'topic-neighbour'),
       ("G3 spike: the dogfood graph had 386 concepts in one connected component, with Logic and Observation concepts as well linked as Resources.", 'polarity', 'contradicts')],
  facets=["386 concepts, 41 components", "37% of Logic, 30% of Observation", "derive writes only CoOccurrence cliques"])
C('g3-02', 'Observation',
  "Recall phase-2 expansion follows out-edges only: 248 of 386 single seeds expand to nothing, and symmetrizing the same BFS multiplies expansion 2 to 4 times for one function call.",
  G3, topic=T,
  q=["Does recall's graph expansion follow in-edges?",
     ("How many single seeds expanded to nothing in G3?", 'fact-lookup', {'g3-01': 1})],
  para="Phase 2 of recall expands only along outgoing edges, so 248 of the 386 seeds tested on their own reached nothing; making the same BFS symmetric would multiply expansion two- to fourfold with a one-function change.",
  rbd=[("Recall phase-2 expansion follows in-edges and out-edges, so every seed with any edge expands.", 'polarity', 'contradicts')],
  facets=["out-edge only", "248/386 expand to nothing", "symmetrizing 2-4x"])
C('g3-03', 'Observation',
  "G3: the exclusivity filter hides pillars. A dependent with path multiplicity is excluded by construction, so 156 of 159 counted pairs sit at identical resistance; the J3-implementation concept had 13 dependents yet blast_radius 4, below the warning bar, while a pendant-6 concept warns.",
  G3, topic=T,
  q=["Why did the J3-implementation concept not trigger a blast-radius warning?",
     ("What does the blast-radius exclusivity filter exclude?", 'fact-lookup')],
  rbd=[("G3 shelved its diffusion hypothesis with numbers: no expansion algorithm can reach what has no edges, which is why diffusion could not beat fixed-depth here.", 'topic-neighbour')],
  facets=["exclusivity filter", "13 dependents, blast_radius 4"])
NA(T, "Did lambo adopt symmetric phase-2 expansion after G3?")
NA(T, "How many components did the dogfood graph have on 2026-09-07?")
NA(T, "What is the average degree of a Logic concept in the dogfood graph?")
