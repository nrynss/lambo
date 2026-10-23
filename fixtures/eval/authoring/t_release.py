from common import C, NA
CH = 'CHANGELOG.md'
T = 'performance'
C('prf-01', 'Observation',
  "0.7.0 (#171): keyword recall p50 went from 18.7 ms to 7.5 ms, and 16 concurrent clients from 63 to 1,104 recalls per second, measured end to end over MCP on a 9,500-concept SQLite store.",
  CH, topic=T,
  q=["How much faster did keyword recall get in 0.7.0?",
     ("How many recalls per second do 16 concurrent clients get after #171?", 'fact-lookup')],
  para="In 0.7.0, #171 cut keyword recall p50 from 18.7 ms to 7.5 ms and lifted throughput with 16 concurrent clients from 63 to 1,104 recalls per second, end to end over MCP against a 9,500-concept SQLite store.",
  rbd=[("0.4.0 (#8, #14): warm recall p50 went from 219 ms to 4.3 ms on a 3,600-concept SQLite session because the session holder ranks vectors from memory.", 'topic-neighbour'),
       ("0.7.0 (#171): keyword recall p50 went from 18.7 ms to 7.5 ms, measured on a 9,500-concept Postgres store over HTTP.", 'repository', 'distinct_entity')],
  facets=["18.7 ms to 7.5 ms", "63 to 1,104"])
C('prf-02', 'Observation',
  "0.7.0 (#171): a back-to-back lambo_derive is acknowledged in 0.8 ms p50 instead of 24.8 ms and applied in 6.3 ms instead of 32.4 ms, because a wake-triggered daemon cycle within 25 ms of the previous one waits out the gap and the burst shares one rescore.",
  CH, topic=T,
  q=["Why are back-to-back writes acknowledged faster in 0.7.0?",
     ("How long can the score table trail a write burst after #171?", 'contrast')],
  para="#171 in 0.7.0 brought a back-to-back lambo_derive's ack from 24.8 ms to 0.8 ms p50 and its apply from 32.4 ms to 6.3 ms: a wake-triggered daemon cycle starting within 25 ms of the last one now waits out that gap, so a burst of writes shares one rescore.",
  rbd=[("0.7.0 (#171): a rescore walks each concept's incident edges once instead of three times, 201 ms to 31 ms per rescore of a 9,100-concept session.", 'topic-neighbour'),
       ("0.7.0 (#171): a back-to-back lambo_derive is acknowledged in 8 ms p50 instead of 24.8 ms because wake-triggered cycles are skipped entirely during a burst.", 'quantity', 'contradicts')],
  facets=["0.8 ms instead of 24.8 ms", "25 ms gap", "one rescore per burst"])
C('prf-03', 'Observation',
  "0.7.0 (#171): lambo provision records a clean #148 re-key pass in store_meta (canonical_key_rule) and later provisions skip the scan; a pass that leaves a collision records nothing, and --rekey forces the full pass. On a 9,508-concept store provision takes 5.6 ms instead of 143 ms.",
  CH, topic=T,
  q=["Why is lambo provision faster in 0.7.0?",
     ("canonical_key_rule", 'exact-identifier'),
     ("Does provision skip the re-key scan if the last pass found a collision?", 'contrast')],
  para="Since #171, once a #148 re-key pass finishes without collisions, provision writes canonical_key_rule into store_meta and later provisions skip the scan; a pass with a collision writes nothing, so the store keeps being checked, and --rekey forces it. Provision on a 9,508-concept store dropped from 143 ms to 5.6 ms.",
  rbd=[("lambo provision skips the re-key scan once it has run, even when that pass left a collision; --rekey is the only way to check again.", 'exception', 'contradicts')],
  facets=["canonical_key_rule in store_meta", "collision records nothing", "5.6 ms instead of 143 ms"])
C('prf-04', 'Observation',
  "SQLite vectors are packed f32 since 0.7.0 (#55): on a 3,600-concept store at dim 1024 the file shrank from 47.3 MB as text to 17.1 MB, and decoding every vector from 161 ms to 3.4 ms; round-trips are bit-exact.",
  CH, topic=T,
  q=["How much smaller are packed f32 vectors on SQLite?",
     ("Are SQLite packed vectors lossy?", 'contrast')],
  para="#55 in 0.7.0 stores SQLite vectors as packed f32: for 3,600 concepts at 1024 dimensions the file went from 47.3 MB of text to 17.1 MB and decoding all vectors from 161 ms to 3.4 ms, with bit-exact round-trips.",
  rbd=[("SQLite vectors are packed f16 since 0.7.0, halving the file again at a small, measured precision cost.", 'identifier', 'contradicts')],
  facets=["47.3 MB to 17.1 MB", "161 ms to 3.4 ms", "bit-exact"])
C('prf-05', 'Observation',
  "Structural recall is fast again in 0.7.0 (#165): anchor resolution Unicode-normalized every concept per query, about 260 ms on a 9,100-concept store; ASCII text now skips normalization, so a query takes about 1 ms instead of about 70 ms in a release build.",
  CH, topic=T,
  q=["Why was structural recall slow in 0.6.0?",
     ("What does #165 skip for ASCII concepts?", 'fact-lookup')],
  rbd=[("Structural recall in 0.7.0 normalizes every concept on every query, including ASCII, but caches the normalized text per concept.", 'polarity', 'contradicts')],
  facets=["Unicode normalization per query", "ASCII skips it", "about 1 ms"])
C('prf-06', 'Observation',
  "0.7.0 (#171): the cosine kernel accumulates in 16 independent lanes in a fixed order, so the compiler vectorizes it and every target scores the same bits; the vector scan sorts only the top k.",
  CH, topic=T,
  q=["Why does the 16-lane cosine kernel give identical scores on every target?"],
  rbd=[("0.7.0 (#171): the cosine kernel uses platform SIMD intrinsics, so scores can differ by target in the last bits.", 'polarity', 'contradicts')],
  facets=["16 lanes", "fixed order"])
NA(T, "What is lambo's recall p50 on a 100,000-concept Postgres store?")
NA(T, "How much memory does a 9,500-concept SQLite session use?")
NA(T, "What is the write throughput of lambo on CockroachDB with 32 agents?")

T = 'releases'
C('rel-01', 'Observation',
  "0.5.0 (#116): with 8 agents, 0.4.0 failed 249 of 519 writes ('hybrid derive could not commit after 8 concurrent graph changes'); 0.5.0 applied 522 of 522, and with 16 agents 1,999 of 1,999, none failed or dropped.",
  CH, topic=T,
  q=["Why did concurrent writes fail on 0.4.0?",
     ("How many writes failed with 8 agents on 0.4.0?", 'fact-lookup')],
  para="#116 in 0.5.0 fixed concurrent writers: at 8 agents 0.4.0 had failed 249 of 519 writes with 'hybrid derive could not commit after 8 concurrent graph changes', while 0.5.0 applied all 522, and at 16 agents all 1,999 with nothing failed or dropped.",
  rbd=[("0.5.0 (#116): with 16 agents, 0.4.0 failed 249 of 519 writes; 0.5.0 still drops a few writes at 16 agents under the 120 s mix.", 'quantity', 'contradicts')],
  facets=["249 of 519 failed", "522 of 522", "1,999 of 1,999"])
C('rel-02', 'Observation',
  "0.5.0 (#102): a derive's commit write-lock hold fell from 6.8 ms to 16.5 µs at 10,000 concepts, from 36.2 ms to 17 µs at 50,000, and from 168 ms to 21 µs at 100,000; every recall and write on the session used to wait behind it.",
  CH, topic=T,
  q=["How long did a derive's commit hold the session write lock before 0.5.0?"],
  rbd=[("0.5.0 (#102): a derive's commit write-lock hold fell from 168 ms to 21 µs at 10,000 concepts.", 'quantity', 'contradicts')],
  facets=["168 ms to 21 µs at 100,000"])
C('rel-03', 'Constraint',
  "0.5.0 (#116 review): a parent_of pair or a record_action edge that would close a cycle in an acyclic relationship is refused with nothing written, and the caller is told why.",
  CH, topic=T,
  q=["What happens when a write would create a parent_of cycle?"],
  para="Since 0.5.0, if a parent_of pair or a record_action edge would close a cycle in a relationship that must stay acyclic, lambo refuses the whole write, stores nothing, and tells the caller why.",
  rbd=[("0.5.0: a parent_of pair that would close a cycle is written, and the daemon breaks the cycle at the next sweep by dropping the newest edge.", 'polarity', 'contradicts')],
  facets=["refused", "nothing written"])
C('rel-04', 'Constraint',
  "0.5.0 (#117): a write the close could not flush answers the receipt state close_unflushed instead of 'durable intent', and a lease refusal reports the lease row's facts (last refresh, expiry) instead of claiming the holder is alive.",
  CH, topic=T,
  q=[("close_unflushed", 'exact-identifier'),
     ("Does a lease refusal claim the holder is alive?", 'contrast')],
  rbd=[("0.5.0: a write the close could not flush still answers 'durable intent', since it will be replayed at the next attach.", 'correction', 'supersedes')],
  facets=["close_unflushed", "lease row's facts"])
C('rel-05', 'Observation',
  "0.4.0 (#22) added the embeddinggemma2 embedder (feature embed-eg2), which runs EmbeddingGemma 2 on llama-server and embeds text, and images when the server is started with --mmproj, into one space. Every image is sent as a lossless PNG whose longer side is 768 px.",
  CH, topic=T,
  q=["How does lambo embed images?",
     ("Which llama-server flag enables image embedding for EmbeddingGemma 2?", 'exact-identifier'),
     ("What canonical form is an image sent in before embedding?", 'fact-lookup')],
  para="Image memory arrived in 0.4.0 (#22) through the embeddinggemma2 embedder, feature embed-eg2, which drives EmbeddingGemma 2 on llama-server; with --mmproj the server embeds images as well as text into the same space, and each image is normalised to a lossless 768-px-longest-side PNG.",
  rbd=[("0.4.0 (#22): images are embedded by the candle BGE-M3 adapter after OCR, and sent as JPEG at 1024 px.", 'polarity', 'contradicts'),
       ("Under EmbeddingGemma 2 with --image-min-tokens 280 --image-max-tokens 280, llama.cpp resizes images to a grid of 48 px cells, and the same picture embedded at 512 and at 1024 px came out at cosine 0.97 to 0.999, not 1.", 'topic-neighbour')],
  facets=["embeddinggemma2 / embed-eg2", "--mmproj", "lossless PNG, 768 px"])
C('rel-06', 'Constraint',
  "0.4.0 (#101): stdio and the session endpoint refuse a request over 4 MiB before parsing it, as HTTP already did.",
  CH, topic=T,
  q=["What is the request size cap on lambo transports?"],
  rbd=[("0.4.0 (#101): stdio refuses a request over 16 MiB; HTTP has no cap.", 'quantity', 'contradicts')],
  facets=["4 MiB", "before parsing"])
C('rel-07', 'Constraint',
  "0.4.0 (#23): lambo erase-session erases a whole session in one transaction on every store and leaves a tombstone that no writer can take over.",
  CH, topic=T,
  q=["How do I delete a whole lambo session?",
     ("Can a writer reuse an erased session's name?", 'contrast')],
  para="Since 0.4.0, lambo erase-session removes an entire session in a single transaction on any store and leaves behind a tombstone no writer can claim.",
  rbd=[("0.4.0 (#23): lambo erase-session deletes a session's concepts but keeps its ledger, and a new writer can attach to the empty session.", 'exception', 'contradicts')],
  facets=["one transaction", "tombstone"])
C('rel-08', 'Constraint',
  "0.4.0 (#4, #92): lambo serve-web serves an allowlist of sessions; each [[web.credential]] token has its own read scope, and without a credential the portal checks Host against DNS rebinding and sends a strict Content-Security-Policy.",
  CH, topic=T,
  q=["How does lambo serve-web protect against DNS rebinding?",
     ("[[web.credential]]", 'exact-identifier')],
  rbd=[("[[serve.credential]] tokens reach only their own sessions on the multi-session HTTP serve, which attaches sessions on demand.", 'topic-neighbour')],
  facets=["allowlist", "Host check", "Content-Security-Policy"])
C('rel-09', 'Constraint',
  "0.4.0 (#18): [recall] kind = \"elastic\" (feature recall-elastic) mirrors each flush to an Elasticsearch index that serves recall's vector leg; the store stays the source of truth.",
  CH, topic=T,
  q=["Can lambo use Elasticsearch for recall?",
     ("Is the Elasticsearch index the source of truth?", 'contrast')],
  rbd=[("[recall] kind = \"elastic\" makes Elasticsearch the source of truth and the store a cache rebuilt from it.", 'polarity', 'contradicts')],
  facets=["recall-elastic", "store stays the source of truth"])
C('rel-10', 'Observation',
  "0.5.0 (#111): embed-candle-cuda builds against CUDA 13.4 directly and no longer needs CUDARC_CUDA_VERSION=13030.",
  CH, topic=T,
  q=[("CUDARC_CUDA_VERSION", 'exact-identifier')],
  rbd=[("0.5.0 (#111): embed-candle-cuda needs CUDARC_CUDA_VERSION=13040 to build against CUDA 13.4.", 'identifier', 'contradicts')],
  facets=["CUDA 13.4", "no CUDARC_CUDA_VERSION"])
NA(T, "When is lambo 0.8.0 planned for release?")
NA(T, "Which Linux distributions ship lambo packages?")
NA(T, "Does lambo publish a Docker image, and under which registry?")
NA(T, "How many downloads has lambo had on crates.io?")

T = 'sessions'
C('ses-01', 'Constraint',
  "0.6.0 (#33): a serve can hold one session per project and layer a shared session such as general under each ([serve] layers, per-project URLs /mcp/s/<session>); recall merges the layers and says which session each hit came from.",
  CH, topic=T,
  q=["How does lambo share memory across projects?",
     ("/mcp/s/<session>", 'exact-identifier')],
  para="Since 0.6.0 (#33) one serve can keep a session per project with a shared session like general layered under each one, configured by [serve] layers and reached at /mcp/s/<session>; recall merges the layers and labels each hit with its session.",
  rbd=[("0.6.0 (#33): lambo session split writes the target sessions into a new SQLite file with exact accounting, and re-runs write nothing.", 'topic-neighbour')],
  facets=["one session per project", "shared layer", "hit names its session"])
C('ses-02', 'Constraint',
  "A shared-layer hit must be similar to the query by vector; a keyword match alone no longer admits it (#157, floor 0.50).",
  CH, topic=T,
  q=["Can a keyword-only match bring in a hit from the shared layer?",
     ("What vector floor applies to shared-layer hits?", 'fact-lookup')],
  para="Since #157 a hit from the shared layer needs vector similarity to the query, at least 0.50; matching on keywords alone is no longer enough to admit it.",
  rbd=[("A shared-layer hit is admitted by a keyword match alone; the 0.50 vector floor applies only to the project's own hits.", 'exception', 'contradicts'),
       ("A shared-layer hit must reach cosine 0.85 to be admitted, the same as the merge threshold.", 'quantity', 'contradicts')],
  facets=["vector similarity required", "floor 0.50"])
C('ses-03', 'Constraint',
  "0.6.0 (#129): with repo_scope, README.md in two repositories is two concepts, not one hub; recall and inspect show the repository, and an ambiguous lookup is refused rather than guessed.",
  CH, topic=T,
  q=["How does lambo keep README.md from two repositories apart?",
     ("What does lambo do with an ambiguous path lookup under repo_scope?", 'contrast')],
  para="#129 in 0.6.0 added repo_scope: the same path, like README.md, in two repositories becomes two concepts instead of one hub, recall and inspect name the repository, and an ambiguous lookup is refused instead of guessed.",
  rbd=[("With repo_scope, README.md in two repositories stays one concept and the repository is recorded as an attribute on each edge.", 'repository', 'contradicts')],
  facets=["two concepts, not one hub", "ambiguous lookup refused"])
NA(T, "How many sessions does the dogfood serve hold?")
NA(T, "Can a shared layer itself be layered over another shared layer?")

T = 'contradictions'
C('con-01', 'Constraint',
  "Unreleased (#179): any concept that a newer concept contradicts, more recently than anything that supports it, carries the Contradicted warning. Before, against only flagged a Canonical or Venerable concept.",
  CH, topic=T,
  q=["Does recall warn when ordinary memory is contradicted?",
     ("Which concepts carry the Contradicted warning after #179?", 'superseded', {'con-04': 1})],
  para="With #179 (unreleased), the Contradicted warning appears on any concept contradicted by a newer one, provided nothing supports it more recently; previously only Canonical or Venerable concepts were flagged.",
  rbd=[("0.6.0 (#19): recall warns only when a Canonical or Venerable concept is contradicted by something newer than its support; contradicting ordinary memory is just memory.", 'correction', 'supersedes')],
  facets=["any contradicted concept warns", "newer than its support"])
C('con-02', 'Constraint',
  "Unreleased (#179): an ordinary contradicted hit must earn its own place in the first top_k; when it does, its contradicting concept is added beside it, past top_k if needed, at most one extra hit per contradicted hit, carrying companion_of.",
  CH, topic=T,
  q=["Can a recall return more than top_k hits?",
     ("companion_of", 'exact-identifier'),
     ("Is a Canonical contradicted hit treated the same as an ordinary one?", 'contrast', {'con-04': 1})],
  rbd=[("Unreleased (#179): a Canonical or Venerable contradicted hit is kept past top_k, and the contradicting concept is shown beside it.", 'exception', 'distinct_entity')],
  facets=["ordinary hit must earn top_k", "companion added past top_k", "at most one extra"])
C('con-03', 'Constraint',
  "Unreleased (#197): GC no longer collects the correction of a contradicted ordinary concept; a sweep protects the correction of every contradiction the detector reports, as long as the contradicted concept is still in memory. Concepts that only contradict each other keep none of themselves alive.",
  CH, topic=T,
  q=["Can GC collect a correction while the stale fact is still in memory?",
     ("Do two concepts that contradict each other keep each other alive?", 'contrast')],
  para="Since #197 (unreleased) a GC sweep keeps the correction of any contradiction the detector reports for as long as the contradicted concept remains; mutual contradictions, in a pair or a longer cycle, protect nothing.",
  rbd=[("0.6.0: GC protects a concept that contradicts a Venerable or Canonical concept until that target is demoted; contradicting ordinary memory protects nothing.", 'correction', 'supersedes')],
  facets=["correction protected while the contradicted concept lives", "mutual contradictions keep nothing alive"])
C('con-04', 'Constraint',
  "0.6.0 (#19): against on lambo_derive and lambo_record_action records that a write shows an existing concept to be wrong. A contradiction never changes a score or a status and no longer counts as a GC connection.",
  CH, topic=T,
  q=["How does an agent record that a concept is wrong?",
     ("Does a contradiction lower a concept's score?", 'contrast')],
  rbd=[("0.6.0 (#19): against on lambo_derive lowers the contradicted concept's score by half and demotes it one status.", 'polarity', 'contradicts')],
  facets=["against", "never changes a score or status"])
C('con-05', 'Constraint',
  "Unreleased (#196): contradictions get a hot-list budget, hot_list_contradiction_max (default half of hot_list_max); over it the lowest-priority contradictions are dropped first, and a full budget keeps the most recently declared ones.",
  CH, topic=T,
  q=["Can contradictions crowd other warnings out of the hot list?",
     ("hot_list_contradiction_max", 'exact-identifier')],
  para="#196 (unreleased) caps contradictions on the hot list at hot_list_contradiction_max, half of hot_list_max by default; past the cap the lowest-priority ones go first, and when full it keeps the most recently declared contradictions.",
  rbd=[("Unreleased (#196): contradictions get a hot-list budget equal to hot_list_max, and a full budget keeps the oldest declared contradictions.", 'quantity', 'contradicts')],
  facets=["default half of hot_list_max", "lowest priority dropped first", "most recently declared kept"])
C('con-06', 'Constraint',
  "0.6.0 (#20): with canonization_supersede = true (off by default), a contradicting concept that earns its own newer evidence demotes the Canonical concept it contradicts, and the audit records why.",
  CH, topic=T,
  q=["Can a contradiction demote a Canonical concept?",
     ("canonization_supersede", 'exact-identifier')],
  rbd=[("Supersession is on by default: any contradiction of a Canonical concept demotes it immediately.", 'polarity', 'contradicts')],
  facets=["off by default", "needs its own newer evidence"])
NA(T, "How many live contradictions does the dogfood store hold?")
NA(T, "Can an agent retract an against it recorded by mistake?")
NA(T, "Does lambo notify a human when a Canonical concept is contradicted?")

T = 'merge-guards'
C('mg-01', 'Constraint',
  "Unreleased (#193): hybrid derive no longer merges a new concept by vector similarity into a concept whose text names different identifiers (digit runs or identifier tokens such as v0.5.0, src/mcp/server.rs, round3), so 'release v0.5.0 is current' and 'release v0.6.0 is current' stay two facts at any cosine.",
  CH, topic=T,
  q=["Why do 'release v0.5.0 is current' and 'release v0.6.0 is current' not merge?",
     ("same_identifiers", 'exact-identifier'),
     ("Can two concepts that differ only in a version number merge by similarity?", 'contrast')],
  para="Since #193 (unreleased), similarity merging in hybrid derive is refused when the two texts name different identifiers, meaning digit runs or tokens like v0.5.0, src/mcp/server.rs or round3; 'release v0.5.0 is current' and 'release v0.6.0 is current' remain separate at any cosine.",
  rbd=[("Unreleased (#193): a new concept is never merged into a concept that its own against names; the Contradicts edge is still written, but no Semantic edge beside it.", 'topic-neighbour'),
       ("Hybrid derive merges 'release v0.5.0 is current' into 'release v0.6.0 is current' when their cosine passes 0.85, keeping the newer version string.", 'polarity', 'contradicts')],
  facets=["different identifiers never merge", "v0.5.0 vs v0.6.0 stay two facts"])
C('mg-02', 'Constraint',
  "0.7.0 (#178): lambo canon alias-preview --similar suggests alias pairs from 3-gram MinHash and vector cosine, with identifier, digit, type and contradiction guards and an accept/reject overrides file; it only suggests, nothing is merged.",
  CH, topic=T,
  q=["Does lambo canon alias-preview --similar merge anything?",
     ("How does alias-preview find near-duplicate pairs?", 'fact-lookup')],
  para="Added in 0.7.0 (#178), lambo canon alias-preview --similar proposes alias pairs using 3-gram MinHash plus vector cosine, guarded by identifier, digit, type and contradiction checks, with an overrides file to accept or reject; it never merges anything itself.",
  rbd=[("lambo canon alias-preview --similar merges every suggested pair above cosine 0.9 unless the overrides file rejects it.", 'polarity', 'contradicts')],
  facets=["MinHash and cosine", "guards", "only suggests"])
C('mg-03', 'Constraint',
  "0.7.0 (#148): canonical keys no longer stem identifier tokens, so distinct identifiers no longer merge through the key. Run lambo provision once with no session holder: it re-keys older concepts in one transaction, refuses while any session lease is live, and a rerun changes nothing; concepts the old rule had already merged stay merged.",
  CH, topic=T,
  q=[("What did #148 change about canonical keys, and what must an operator run?", 'fact-lookup', {'prf-03': 1}),
     ("Does the #148 re-key split concepts that were already merged?", 'contrast')],
  para="Since 0.7.0 (#148) canonical keys keep identifier tokens unstemmed, so different identifiers can no longer collapse onto one key. lambo provision, run once with no session holder, re-keys older concepts in a single transaction, refuses while a session lease is live and is a no-op on rerun; anything the old rule already merged stays merged.",
  rbd=[("0.7.0 (#148): lambo provision splits concepts that the old key rule had merged back into one concept per identifier.", 'exception', 'contradicts')],
  facets=["keys no longer stem identifier tokens", "provision re-keys once", "already merged stay merged"])
NA(T, "How many merges did #193's guards refuse on the dogfood store?")
NA(T, "Which identifier tokenizer does alias-preview use for Chinese text?")

T = 'canonization'
C('cn-01', 'Constraint',
  "0.6.0 (#36): canonization promotes rulings, not artifacts: support comes from the concepts that depend on a ruling, counted by their distinct origin interactions with the ruling's own origin excluded, so several dependents from one interaction count once.",
  CH, topic=T,
  q=["What counts as support for canonizing a ruling?",
     ("Do several dependents from one interaction count as separate support?", 'contrast')],
  para="Since #36 in 0.6.0, canonization promotes rulings rather than artifacts. Support is the set of concepts depending on the ruling, counted by distinct origin interactions excluding the ruling's own, so multiple dependents from a single interaction count once.",
  rbd=[("Before 0.6.0, canonization support counted the actions that touched a file, so a frequently edited file could become Canonical.", 'correction', 'supersedes')],
  facets=["dependents' distinct origin interactions", "own origin excluded", "one interaction counts once"])
C('cn-02', 'Constraint',
  "Agents cite rulings by id in depends_on; lambo canon dry-run and alias-preview show the effect on a backup before it happens.",
  CH, topic=T,
  q=["How do agents cite a ruling?", ("lambo canon dry-run", 'exact-identifier')],
  facets=["depends_on by id", "dry-run on a backup"])
NA(T, "How many Canonical concepts does the dogfood store have?")
NA(T, "What is the Venerable promotion threshold in support count?")
