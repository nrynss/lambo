from common import C, NA, D, P
SUITE = 'nrynss/lambo-paper:evaluation/results/2026-09-07/lambo-2026-09-07/suite.json'
JMC = 'dev-diary/lambo-for-mooshik/J-multi-client.md'
T = 'dogfood-topology'
C('top-01', 'Constraint',
  "The Mac dogfood writer is a single launchd-supervised lambo serve over HTTP on port 7700, running as agent http-shared-writer; every client connects to http://127.0.0.1:7700/mcp instead of spawning its own serve.",
  SUITE, topic=T,
  q=[("How is the Lambo dogfood writer deployed on the Mac, and how do agents connect to it?", 'multi-hop', {'top-02': 1}),
     ("http://127.0.0.1:7700/mcp", 'exact-identifier'),
     ("Should each agent on the dogfood machine start its own stdio serve?", 'contrast')],
  para="On the Mac, dogfood writes go through one lambo serve kept alive by launchd, listening over HTTP on port 7700 as agent http-shared-writer. Clients do not start their own serve; they all point at http://127.0.0.1:7700/mcp.",
  rbd=[("Each agent on the Mac dogfood machine spawns its own stdio lambo serve, and the serves coordinate through the store's single-writer lease.", 'correction', 'supersedes'),
       ("The isolated evaluation copy of the dogfood store was served on port 7701 over HTTP, so the live writer on 7700 was never touched.", 'identifier', 'distinct_entity')],
  facets=["single launchd lambo serve", "HTTP port 7700", "clients connect to http://127.0.0.1:7700/mcp"])
C('top-02', 'Resource', D("Re-pinned the dogfood binary to lambo-4c6fc93"), topic=T,
  q=[("What is the launchd unit name for the dogfood writer?", 'fact-lookup', {'top-03': 2}),
     ("How many nodes did the dogfood graph have at the lambo-4c6fc93 re-pin?", 'fact-lookup'),
     ("dev.lambo.dogfood", 'exact-identifier', {'top-03': 1})],
  facets=["dev.lambo.dogfood", "lease holder http-shared-writer"])
C('top-03', 'Logic',
  "The launchd ThrottleInterval for dev.lambo.dogfood is 60 seconds because the default 10 s restart falls inside the 45 s single-writer lease TTL: a restarted serve would find the lease still held and exit. At 60 s every retry lands after the lease has lapsed.",
  SUITE, topic=T,
  q=[("Why is the launchd ThrottleInterval for the dogfood unit 60 seconds instead of the default?", 'rationale', {'srv-01': 1}),
     ("What goes wrong if launchd restarts the dogfood serve after 10 seconds?", 'vocabulary-mismatch')],
  para="dev.lambo.dogfood uses a 60-second launchd ThrottleInterval. With the default 10-second restart, the new serve would start while the 45-second writer lease from the dead one was still held and would exit; 60 seconds guarantees the lease has expired before each retry.",
  rbd=[("The launchd ThrottleInterval for dev.lambo.dogfood is 60 seconds simply to avoid a tight restart loop when the binary crashes at start; the lease plays no part.", 'correction', 'contradicts'),
       ("The launchd ThrottleInterval for dev.lambo.dogfood is 30 seconds, comfortably above the 15 s lease heartbeat.", 'quantity', 'contradicts')],
  facets=["default 10 s restart", "inside the 45 s lease TTL", "60 s retries after the lease lapses"])
C('top-04', 'Logic', D("Claude Code enforcement layer 2026-09-01"), topic=T,
  q=["How is Claude Code forced to follow the lambo-dogfood protocol?",
     ("What happens to the dogfood hooks when the writer is down?", 'contrast'),
     ("Why can't a restricted-toolset agent deadlock on the recall gate?", 'fact-lookup')],
  rbd=[("Claude Code follows the lambo-dogfood protocol from AGENTS.md text alone, like the other harnesses, so no hooks are needed.", 'polarity', 'contradicts'),
       ("The dogfood hooks fail closed when the writer is down: Edit and Write stay denied until the writer comes back.", 'exception', 'contradicts')],
  facets=["hooks at user scope", "PreToolUse denies edits until one lambo_recall", "fails open when the writer is down"])
C('top-05', 'Observation',
  "Dogfood ledger (the I3 snapshot): recall-first compliance was 0.0% for claude-orchestrator (0 of 22 work sequences began with lambo_recall, and 37 derives had no prior recall), against 35.2% across all agents.",
  'evidence/mooshik-i3-live-ledger/recall_first.txt', topic=T,
  q=[("Did agents call lambo_recall before acting when only told to?", 'fact-lookup', {'top-04': 1, 'fnd-01': 1})],
  rbd=[("Dogfood ledger (the I3 snapshot): j2-implementor and j2-remediation-r1 began every work sequence with lambo_recall, 100% recall-first compliance.", 'topic-neighbour')],
  facets=["0.0% for claude-orchestrator", "35.2% overall"])
NA(T, "What port does the dogfood writer use on the Linux rig?")
NA(T, "How many agents connect to the dogfood writer at peak?")
NA(T, "What is the monthly cost of the lambo-pg Cloud SQL instance used for the hosted Postgres workstream?", family='efficacy',
   note="efficacy question missing-cloud-cost (2026-09-07): absent by design")
NA(T, "Which launchd KeepAlive settings does the dogfood unit use?")

T = 'multi-client'
C('srv-01', 'Constraint',
  "The single-writer lease has a 45 s TTL (LEASE_TTL), refreshed every 15 s (LEASE_HEARTBEAT_INTERVAL), so a holder gets three refresh chances inside one TTL and is meant to lose the lease if it misses all three.",
  JMC, topic=T,
  q=["How long is the single-writer lease TTL and how often is it refreshed?",
     ("LEASE_HEARTBEAT_INTERVAL", 'exact-identifier', {'srv-06': 1})],
  para="LEASE_TTL is 45 seconds and LEASE_HEARTBEAT_INTERVAL is 15 seconds: the writer refreshes three times per TTL and should lose the lease only after missing all three.",
  rbd=[("The single-writer lease has a 15 s TTL, refreshed every 5 s.", 'quantity', 'contradicts')],
  facets=["45 s", "15 s", "three chances"])
C('srv-02', 'Constraint', D("J1 gates everything multi-client"), topic=T,
  q=["Why must per-call agent_id be accepted before a shared writer is honest?",
     ("Why does lambo_reserve refuse every client but one behind a single serve?", 'rationale')],
  para="J1 is the gate for all multi-client work: a serve stamps its own --agent on every connected client, and lambo_reserve refuses any caller with a different agent_id. That fail-closed rule is right, but it leaves all but one client without lambo's only mutual-exclusion primitive, so Memory must accept a per-call agent_id and record it on the interaction before a proxy or shared writer can be honest.",
  rbd=[("lambo_reserve accepts any caller's agent_id as long as the reservation does not overlap another one, so many clients can share one serve without per-call identity.", 'polarity', 'contradicts')],
  facets=["serve applies its own --agent", "lambo_reserve hard-refuses other agent_ids", "per-call agent_id"])
C('srv-03', 'Logic', D("J1 round-2 lesson: matching an error VARIANT"), topic=T,
  q=["Why is matching on an error variant not enough when unhiding errors to a model?",
     ("What did lease_lost_error disclose through conflict_err?", 'fact-lookup')],
  rbd=[("When selectively unhiding an error class to the model, matching on the error variant is enough, because each variant has exactly one producer.", 'polarity', 'contradicts')],
  facets=["variant is not a path", "lease_lost_error is also a Conflict", "discriminate structurally"])
C('srv-04', 'Logic', D("J2 round-3 residual coupling, handed to J3"), topic=T,
  q=["How many un-raced Self::send sites did the J2 round-3 residual find in proxy.rs?",
     ("answer_lost un-raced write burst", 'exact-identifier', {'srv-05': 1})],
  rbd=[(D("J3 resolution of the COUPLED ceiling residual")['text'], 'correction', 'supersedes',
        "#178 labelled this pair related-but-distinct (i3-42); whether J3's resolution supersedes the residual or only narrows it is the owner's call")],
  facets=["six un-raced Self::send sites", "inflight list"])
C('srv-05', 'Logic', D("J3 resolution of the COUPLED ceiling residual"), topic=T,
  q=["How did J3 bound the proxy's in-flight list for receipt waits?",
     ("RECEIPT_WAIT_MAX", 'exact-identifier'),
     ("Does the write-queue bound feed the proxy's inflight list?", 'contrast')],
  facets=["RECEIPT_WAIT_MAX", "concurrent-waiter cap", "INFLIGHT_DEPTH_WARN = 64"])
C('srv-06', 'Logic', D("Register-discipline technique that paid off twice"), topic=T,
  q=["What should you check when you correct a stale claim in a docstring?",
     ("What was wrong with ELECTION_SLACK's stated reason?", 'fact-lookup', {'srv-01': 1})],
  facets=["check every constant the sentence cites", "LEASE_HEARTBEAT_INTERVAL = 15s"])
C('srv-07', 'Constraint',
  "The proxy's DIAL_BUDGET is 6 seconds and bounds the client's wait: past it the dial to the holder is abandoned and the call is answered 'nothing happened, retry later'. It sits above the dial's own connect and replay budgets (about 4.1 s) and below sqlite's 8 s busy_timeout.",
  'src/mcp/proxy/dialing.rs', topic=T,
  q=[("DIAL_BUDGET", 'exact-identifier', {'srv-04': 1}), "How long does a proxying serve wait on a dial to the writer before answering the client?"],
  rbd=[("The proxy's dial budget is 60 seconds, one lease TTL plus slack.", 'quantity', 'contradicts'),
       ("A SIGTERM arriving mid-dial is honoured at the next poll, because the dial is raced against the shutdown future rather than bounded by a budget.", 'topic-neighbour')],
  facets=["6 seconds", "abandoned, retry later"])
NA(T, "What is the maximum number of clients a proxying serve accepts?")
NA(T, "How does lambo elect a new writer across two machines?")
NA(T, "Which J-series review found the lease clock-skew bug?")

T = 'async-ack'
C('ack-01', 'Observation', D("J3 latency claim VERIFIED at the release binary"), topic=T,
  q=["How fast is a lambo_derive ack after J3 compared with before?",
     ("Is the embedder still in the derive call path after J3?", 'vocabulary-mismatch', {'ack-04': 2})],
  para="Measured on 2026-08-20 at the release binary over real stdio MCP (SQLite, 20 calls, median) against the rig's live llama.cpp BGE-M3 q8_0 on CPU: a lambo_derive ack dropped from 14.111 ms on synchronous base 166a3c8 to 0.048 ms with J3, at or under the same binary's 0.076 ms ack against FixtureEmbedder, so the embedder is out of the call. Both runs applied all 21 concepts, so the fast case did real work; the 27 ms in the doc came from the dogfood client stack on pinned 3039b82, and the delta is J3's.",
  rbd=[("After J3 a lambo_derive ack still waits for the embedder, so it costs about 14 ms against a CPU llama.cpp BGE-M3.", 'polarity', 'contradicts')],
  facets=["14.111 ms to 0.048 ms", "below FixtureEmbedder 0.076 ms"])
C('ack-02', 'Logic', D("J3 redesign round-1 review VERDICT: REQUEST_CHANGES"), topic=T,
  q=[("What was the P1 finding in the J3 durable-intent redesign round-1 review?", 'fact-lookup', {'ack-03': 2}),
     ("Did the J3 founding invariant hold at kill -9?", 'contrast')],
  facets=["transient embedder outage at attach burns the backlog", "64 acked"])
C('ack-03', 'Observation', D("Closed J3 durable-intent redesign review round 1"), topic=T,
  q=["Why could the J3 redesign review not be committed?",
     ("macOS TCC revoked filesystem access", 'exact-identifier')],
  facets=["macOS TCC revoked access", "paused before remediation"])
C('ack-04', 'Constraint',
  "J3 acks a derive once its intent is durable in the write queue, before the embedder runs; the receipt (lambo_receipt) later reports applied, refused or lost, and is the only place a write's real outcome appears.",
  'dev-diary/lambo-for-mooshik/J-multi-client.md', topic=T,
  q=["When does lambo acknowledge a derive after J3?",
     ("How does a client learn whether an acked write was applied?", 'vocabulary-mismatch')],
  para="Since J3 a derive is acknowledged as soon as its intent is durably queued, ahead of embedding; the actual outcome (applied, refused or lost) is only visible later through lambo_receipt.",
  rbd=[("J3 acks a derive only after the concept is applied and embedded, so the ack itself is the outcome and receipts are optional.", 'polarity', 'contradicts')],
  facets=["ack once the intent is durable", "lambo_receipt reports the outcome"])
NA(T, "What is the p99 lambo_derive ack latency on Postgres after J3?")
NA(T, "How many durable intents were lost in the dogfood store in September?")

T = 'process'
C('pro-01', 'Constraint',
  "Lambo agent roles: all remediation runs on Opus, and whole-workstream end-to-end review runs on Fable; per-task review also runs on Opus.",
  SUITE, topic=T,
  q=[("Which Claude model runs Lambo remediation agents, and which runs whole-workstream end-to-end review?", 'fact-lookup', {'pro-02': 1}),
     ("Who does the end-to-end adversarial review of a workstream?", 'vocabulary-mismatch')],
  para="Every remediation agent in Lambo runs on Opus, per-task reviews also use Opus, and the end-to-end review of a whole workstream uses Fable.",
  rbd=[("Lambo agent roles: remediation runs on Fable, and whole-workstream end-to-end review runs on Opus.", 'polarity', 'contradicts')],
  facets=["remediation on Opus", "E2E review on Fable"])
C('pro-02', 'Logic', D("Post-J plan (operator, 2026-08-20)"), topic=T,
  q=["What was the planned order of workstreams after J closed?",
     ("Which workstream was the only remaining blocker of C2?", 'fact-lookup')],
  facets=["end-to-end adversarial review over the whole branch", "then workstream D", "D blocks C2"])
C('pro-03', 'Logic', D("Workstream I (observability) runs before"), topic=T,
  q=["Why did observability (workstream I) run before further implementation cycles?",
     ("What is a flow record and why did lambo lack one?", 'vocabulary-mismatch')],
  para="On 2026-08-19 it was decided that workstream I, observability, goes before any further implementation cycle. Lambo already exposes state well (canonization audit rows, stats, the store, serve-web) but kept no record of flow: recall scores disappear with the response and a blast-radius warning lives only in the conversation that saw it, so it can't be verified or claimed. Every cycle run before I lands loses dogfood data.",
  facets=["decided 2026-08-19", "no flow record", "dogfood data lost"])
C('pro-04', 'Logic', D("Workstream B is extract-then-extend"), topic=T,
  q=["Why did workstream B extract PgStore from the Cockroach adapter instead of copying it?",
     ("When does a function enter the shared PgStore base?", 'fact-lookup')],
  rbd=[("Workstream B is copy-then-edit: the Postgres adapter starts as a copy of the Cockroach adapter and the two are edited independently.", 'polarity', 'contradicts')],
  facets=["extract-then-extend", "byte-identical SQL", "copy-then-edit rejected"])
C('pro-05', 'Logic', D("Alias split: config strings postgres/pg"), topic=T,
  q=["What do the store aliases pg and crdb resolve to?",
     ("What happens if a Postgres config is pointed at Cockroach?", 'contrast', {'pro-04': 1})],
  para="Config names postgres and pg map to PgStore<PostgresDialect>, and cockroach and crdb map to PgStore<CockroachDialect>, with no crossover. The change is recorded and fails loudly rather than mis-ranking: Postgres's CREATE EXTENSION vector and its <=> operator do not exist on Cockroach, so a crossed config dies at provisioning or on the first vector query.",
  rbd=[("Alias split: the config string postgres resolves to PgStore<CockroachDialect> when the server reports Cockroach, so a crossed config keeps working.", 'exception', 'contradicts')],
  facets=["postgres/pg -> PostgresDialect", "cockroach/crdb -> CockroachDialect", "fails loud"])
C('pro-06', 'Observation', D("Committed the J2 round-3 adversarial review on wt/j2"), topic=T,
  q=[("What was the verdict of the J2 round-3 adversarial review?", 'fact-lookup', {'pro-07': 2})],
  facets=["CLEAN", "aefda87"])
C('pro-07', 'Observation', D("J2 round-3 adversarial review (adve-review-mooshik-J2-round3.md)"), topic=T,
  q=[("How many fixtures tests passed at 6442d95 in the J2 round-3 review?", 'fact-lookup')],
  facets=["858/0/3 fixtures", "verify.sh 40 ok"])
C('pro-08', 'Logic', D("J1 round-3 advisory (J1-R3-3, caps.rs not J1): the INVISIBLE_RANGES decision recorded at is_disallowed_format states U+2028/U+2029 meet the table's own 'invisible structural character' criterion, then declines on the ground 'content already permits \\n,"), topic=T,
  q=["Why can a U+2028 in content not fork a canonical key?",
     ("INVISIBLE_RANGES", 'exact-identifier')],
  facets=["normalize_tokens splits on char::is_whitespace", "Default_Ignorable codepoints pass check_size"])
NA(T, "How many adversarial review rounds did workstream C take?")
NA(T, "Who owns workstream E, and what does it deliver?")
NA(T, "How many P1 findings did the full-stack sweep review find?")

# Two records of the same review event, written minutes apart, each with
# details the other lacks. #178 labelled both "paraphrase duplicate"; whether a
# merge (which keeps one text) is acceptable here is the owner's call.
_SAME_EVENT = "same event recorded twice with different details; a merge keeps one text and drops the other's facts"
P(D("Closed J3 durable-intent redesign review round 1")['text'], D("J3 redesign round-1 review VERDICT")['text'],
  'paraphrase', 'same-event', 'evidence/issue-178-similar/labelled-pairs.json#i3-30', review=_SAME_EVENT)
P(D("Committed the J2 round-3 adversarial review on wt/j2")['text'], D("J2 round-3 adversarial review (adve-review-mooshik-J2-round3.md)")['text'],
  'paraphrase', 'same-event', 'evidence/issue-178-similar/labelled-pairs.json#i3-41', review=_SAME_EVENT)
