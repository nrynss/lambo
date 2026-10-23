from common import C, NA, D
T = 'embedding-coverage'
C('emb-01', 'Observation', D("Dogfood finding (2026-08-20, DOGFOOD-FINDINGS.md): 92 of 100"), topic=T,
  q=["How many dogfood concepts were missing embeddings in the first count?",
     ("Were the unembedded dogfood concepts from a single embedder outage window?", 'fact-lookup')],
  para="Finding from DOGFOOD-FINDINGS.md, 2026-08-20: of the 100 concepts in the lambo-dev store, 92 have no durable vector, spread through time rather than grouped in one outage; that covers all 44 Resources written by record_action and most of the Constraints, Logics and Entities. Semantic recall and dedup cannot see them, and the damage keeps accumulating with no way to backfill. Possible product work: an embedding-coverage counter in lambo_stats and a re-embed backfill verb. The cause is open: record_action never embedding by design, an intermittently unavailable embedder dropping hybrid to keyword, or a flush gap.",
  rbd=[("Dogfood finding 2026-08-22: the embedding damage is 475 of 509 concepts, not 92 of 100; the earlier figure is stale and understates the backfill by roughly five times.", 'correction', 'supersedes')],
  facets=["92 of 100", "not one outage window"])
C('emb-02', 'Observation', D("The dogfood store's embedding damage is 475 of 509"), topic=T,
  q=[("What share of the dogfood store was unembedded once it reached about 500 concepts?", 'superseded', {'emb-01': 1}),
     ("Why were recall-quality judgements on the dogfood graph really about the keyword path?", 'rationale')],
  para="475 of the dogfood store's 509 concepts lack a vector, much worse than the 92-of-100 figure in K's motivating table, which is out of date and understates the backfill about fivefold. Two consequences follow. Semantic recall on this store is effectively off: lambo_recall matches by keyword for 93% of the corpus, so any judgement of recall quality made on the dogfood graph is about the keyword path, not hybrid recall. And K2's re-embed verb and the embedded/total counter in lambo_stats matter more: the counter would have shown the drift on its first day, and the verb now repairs 93% of the store instead of a tenth. Coverage should be measured again before any G recall-calibration work, because calibrating against a corpus only 7% embedded would measure the wrong pipeline.",
  facets=["475 of 509", "93%"])
C('emb-03', 'Logic', D("CORRECTION to the same-day claim that the 512-token clamp"), topic=T,
  q=[("Was the 512-token clamp the main reason dogfood concepts lacked vectors?", 'contrast', {'emb-04': 1}),
     "What evidence showed that short dogfood concepts were unembedded too?"],
  rbd=[("The 512-token llama-server clamp is a plausible contributor to the 92 of 100 unembedded dogfood concepts: any input over about 512 tokens fails with HTTP 500 and, on a pre-J3 binary, the write applied with a NULL embedding.", 'correction', 'supersedes')],
  facets=["not the dominant cause", "2/247"])
C('emb-04', 'Constraint', D("The dogfood embedder's capacity flags are LOAD-BEARING"), topic=T,
  q=["Which llama-server flags does the BGE-M3 dogfood embedder need?",
     ("Why is /health not enough to check the llama embedding server?", 'rationale'),
     ("--ubatch-size 8192", 'exact-identifier')],
  rbd=[("The dogfood llama-server's capacity flags are tuning only: the defaults handle every input and only change throughput.", 'polarity', 'contradicts'),
       ("The dogfood llama-server must be started with --batch-size 4096 --ubatch-size 4096 -c 4096, and -u is accepted as the short form.", 'identifier', 'contradicts')],
  facets=["--batch-size 8192", "--ubatch-size 8192", "verify with a >512-token embed"])
C('emb-05', 'Logic', D("J3 round-3 residual handed outside J3"), topic=T,
  q=["Does an applied receipt prove the concept was embedded?",
     ("Where should an end-to-end durability test check that a write was embedded?", 'vocabulary-mismatch')],
  rbd=[("A concept applied with a NULL embedding is flagged in its receipt as 'applied (unembedded)' and a warning names the failed embed, so the ack alone tells the caller.", 'polarity', 'contradicts')],
  facets=["NULL embedding is unfindable", "assert on the store"])
C('emb-06', 'Resource', D("Re-pinned the dogfood binary to lambo-e11fb06"), topic=T,
  q=[("What did the record_action embedder fix change in the dogfood store?", 'fact-lookup', {'emb-07': 2}),
     ("How many NULL-vector concepts did re-embed --missing-only backfill on the dogfood rig?", 'fact-lookup'),
     ("lambo-e11fb06", 'exact-identifier')],
  para="The dogfood binary was moved to lambo-e11fb06 (lambo-for-mooshik at e11fb06, clean tree, LAMBO_GIT_SHA set, features store-sqlite, embed-candle-metal, embed-bge). That pin brings the fix that makes record_action go through the embedder (its concepts used to be stored with a NULL vector, hiding everything agents did from semantic recall) and the re-embed --missing-only repair verb. With serve stopped, the repair filled 42 NULL vectors, taking coverage from 534/576 to 576/576 under the unchanged candle BAAI/bge-m3 1024-dim contract; launchd was repointed from lambo-4c6fc93, restarted, and checked: lease http-shared-writer pid 35655, stats embedded=576/576, heartbeat git_sha=e11fb06.",
  rbd=[(D("Re-pinned the dogfood binary to lambo-4c6fc93")['text'], 'identifier', 'supersedes')],
  facets=["record_action stored NULL embeddings", "42 backfilled", "576/576"])
C('emb-07', 'Observation',
  "record_action created its Resource and Entity concepts with NULL embeddings because it had no embedder hop, so everything an agent did was reachable only by keyword or graph traversal while derive-created concepts were vector-searchable, an inversion of what recall should favour. Commit bef53e6 added the hop.",
  'paper evaluation suite lambo-2026-09-07 record-action-embedding rubric', topic=T,
  q=[("What defect did the record_action embedder fix in commit bef53e6 correct?", 'fact-lookup', {'emb-06': 1}),
     ("Why could agents not semantically recall the actions they had recorded?", 'vocabulary-mismatch', {'emb-06': 1})],
  para="Before commit bef53e6, record_action never called the embedder, so the Resource and Entity concepts it wrote had NULL vectors. Action records could only be found by keyword or graph walks, while concepts written by derive were vector-searchable, which inverted the intended recall behaviour.",
  rbd=[("derive created its concepts with NULL embeddings while record_action Resources were embedded, so derived knowledge was keyword-only until the fix.", 'polarity', 'contradicts')],
  facets=["NULL embeddings", "no embedder hop", "inversion"])
C('emb-08', 'Observation',
  "The 184 historical dogfood pairs at or above the 0.85 merge threshold never merged (semantic_merged stayed 0) because a pair cannot merge when the older concept had no embedding at the time the newer one was written; the embedding gap disabled deduplication as well as semantic recall.",
  'evidence/mooshik-i3-live-ledger/README.md', topic=T,
  q=["Why did the dogfood store keep 184 near-duplicate pairs above the merge threshold?",
     ("Why was semantic_merged zero on the dogfood ledger?", 'vocabulary-mismatch')],
  para="Because the older half of each pair had no vector when the newer half was written, none of the 184 dogfood pairs scoring 0.85 or more ever merged, and semantic_merged stayed at 0. Missing embeddings broke deduplication, not only semantic recall.",
  rbd=[("In the dogfood store 4,962 concept pairs sit in the cosine band from 0.65 to 0.85, below the merge threshold and unlabelled.", 'topic-neighbour')],
  facets=["184 pairs", "older concept had no embedding"])
NA(T, "What was the embedding coverage of the dogfood store on 2026-09-15?")
NA(T, "Which agent wrote the most unembedded concepts in the dogfood store?")
NA(T, "How long did lambo re-embed take to backfill the 42 dogfood concepts?")
NA(T, "Did the dogfood store lose any concepts during the re-embed backfill?")

T = 'candle'
K = 'dev-diary/lambo-for-mooshik/K-candle-embedder.md'
C('cnd-01', 'Logic',
  "Workstream K moves BGE-M3 in-process through candle instead of a llama.cpp server: with no transport, 'embedder unreachable' stops being a runtime state, and what remains is a startup condition such as missing or corrupt weights.",
  K, topic=T,
  q=["Why embed in-process with candle rather than through llama.cpp?",
     ("What failure mode does an in-process embedder remove?", 'vocabulary-mismatch')],
  para="K runs BGE-M3 inside the lambo process via candle rather than calling a llama.cpp server, so the embedder can no longer become unreachable mid-run; the failures left are deterministic at startup, like absent or damaged weights.",
  rbd=[("A hosted embedder such as Gemini fails over the network by definition, so it keeps needing J3's classification of transport failures.", 'topic-neighbour')],
  facets=["no transport", "startup condition"])
C('cnd-02', 'Entity',
  "The candle adapter loads BGE-M3 weights from nrynss/bge-m3-f16-safetensors on Hugging Face, an f16 cast of BAAI/bge-m3 verified tensor by tensor (391 of 391 tensors f16-equal).",
  'src/embed/candle.rs', topic=T,
  q=[("nrynss/bge-m3-f16-safetensors", 'exact-identifier'),
     "Where do the candle BGE-M3 weights come from?"],
  para="Candle's BGE-M3 weights are fetched from the Hugging Face repo nrynss/bge-m3-f16-safetensors, an f16 cast of BAAI/bge-m3 checked to match it on all 391 tensors.",
  rbd=[("The candle adapter loads BGE-M3 weights directly from BAAI/bge-m3 in f32; there is no f16 cast.", 'polarity', 'contradicts'),
       ("The llama.cpp rig serves a q8_0 GGUF quantisation of BGE-M3 on 127.0.0.1:8080.", 'identifier', 'distinct_entity')],
  facets=["nrynss/bge-m3-f16-safetensors", "391/391 tensors"])
C('cnd-03', 'Constraint',
  "candle device selection: device = auto resolves Metal on Apple silicon and CUDA elsewhere, and refuses to start when neither accelerator exists; device = cpu is the only way to serve without an accelerator.",
  'src/embed/mod.rs', topic=T,
  q=["What does the candle embedder do on a machine with no GPU?",
     ("How do I run the candle embedder on CPU?", 'fact-lookup')],
  para="With device = auto, candle picks Metal on Apple silicon or CUDA otherwise and fails at startup if there is no accelerator; to run without one you must set device = cpu.",
  rbd=[("candle device selection: device = auto falls back to the CPU silently when neither Metal nor CUDA is available.", 'exception', 'contradicts')],
  facets=["auto refuses without an accelerator", "device = cpu"])
C('cnd-04', 'Observation',
  "On Metal, a lambo serve that becomes a proxy no longer loads the model (#31): its embedder takes about 4 MB instead of about 1.4 GB, and a cached weight digest lets a start with an unchanged model read its identity in under 1 ms instead of hashing 1.1 GB.",
  'CHANGELOG.md', topic=T,
  q=["How much memory does a proxying serve's embedder use on Metal after #31?",
     ("Why does starting a serve no longer hash the 1.1 GB weights?", 'rationale')],
  para="Since #31 a serve that ends up proxying skips the model on Metal, holding about 4 MB for its embedder rather than about 1.4 GB, and the weight digest is cached so an unchanged model's identity is read in under a millisecond instead of hashing 1.1 GB.",
  facets=["about 4 MB", "1.4 GB", "digest cached"])
C('cnd-05', 'Logic',
  "K1's first gate is cosine parity: embed the dogfood corpus through the candle spike and through the rig's llama-server and compare; a median self-agreement below 0.99 means the pooling is wrong. Subtly wrong pooling yields plausible but wrong vectors, so recall would degrade silently.",
  K, topic=T,
  q=["What threshold did K1 use for candle versus llama cosine parity?",
     ("Why is pooling correctness the most important gate for an in-process embedder?", 'rationale')],
  rbd=[("K1's second number is throughput on CPU and Metal by the J3 probe method: representative input sizes, warm-up discarded, serial and concurrent legs.", 'topic-neighbour')],
  facets=["median self-agreement 0.99", "wrong pooling degrades silently"])
C('cnd-06', 'Observation',
  "The Mac dogfood rig embeds with in-process candle BGE-M3 on Metal (kind = candle, device = metal, 1024 dimensions). It replaced the llama-server bge_m3 embedder on 127.0.0.1:8080 in the K2 migration on 2026-08-23.",
  'paper evaluation suite lambo-2026-09-07 embedder-superseded rubric', topic=T,
  q=[("Which embedder does the Mac dogfood rig use today, and what did it replace?", 'superseded', {'emb-04': 1}),
     ("When did the dogfood rig stop using llama-server for embeddings?", 'superseded')],
  para="Since the K2 migration on 2026-08-23 the Mac dogfood rig has embedded in-process with candle BGE-M3 on Metal at 1024 dimensions (kind candle, device metal), replacing the llama-server bge_m3 embedder that listened on 127.0.0.1:8080.",
  rbd=[("The Mac dogfood rig embeds through the llama-server bge_m3 embedder on 127.0.0.1:8080, with candle kept only as a fallback.", 'correction', 'supersedes')],
  facets=["candle BGE-M3 on Metal", "replaced llama-server on 127.0.0.1:8080", "2026-08-23"])
NA(T, "What is the candle BGE-M3 throughput on CUDA in embeddings per second?")
NA(T, "Does the candle adapter use BGE-M3's sparse or ColBERT outputs?")
NA(T, "Which candle crate version does lambo pin?")

T = 'canary'
CH = 'CHANGELOG.md'
C('can-01', 'Constraint',
  "Embedder canary (#176): a writer attach embeds 20 fixed phrases and compares them with the vectors pinned for that contract in store_meta; any phrase below cosine 0.99 is a drift, which catches a model change behind the same kind, model and dim.",
  CH, topic=T,
  q=["How does lambo detect a model change behind an unchanged embedding contract?",
     ("What cosine counts as canary drift?", 'fact-lookup'),
     ("store_meta canary pin", 'exact-identifier')],
  para="Since #176 every writer attach re-embeds twenty fixed canary phrases and checks them against the vectors stored for that contract in store_meta; a phrase scoring under 0.99 cosine is a drift, which exposes a swapped model hiding behind an unchanged kind, model and dimension.",
  rbd=[("Embedder canary: a writer attach embeds 10 fixed phrases and treats any phrase below cosine 0.95 as a drift.", 'quantity', 'contradicts'),
       ("The canary pin also records cosine quantiles over pairs of different canary phrases, used as a relevance floor and never as a merge threshold.", 'topic-neighbour')],
  facets=["20 fixed phrases", "0.99", "store_meta"])
C('can-02', 'Constraint',
  "[embedder] canary modes: verify refuses the writer attach on a drift unless --allow-embedding-mismatch is passed, warn (the default) logs it, and off skips the check. An unreachable embedder leaves the check unverified rather than refused.",
  CH, topic=T,
  q=["What happens on canary drift when canary = verify?",
     ("What is the default embedder canary mode?", 'fact-lookup'),
     ("Does an unreachable embedder fail the canary check?", 'contrast')],
  para="The canary setting takes three values: verify blocks a writer attach on drift unless --allow-embedding-mismatch is given, warn only logs and is the default, and off disables the check. If the embedder cannot be reached, the check is left unverified, not failed.",
  rbd=[("[embedder] canary modes: verify is the default and refuses the writer attach on any drift; warn has to be chosen explicitly.", 'polarity', 'contradicts'),
       ("[embedder] canary modes: an unreachable embedder fails the check and refuses the attach under verify.", 'exception', 'contradicts')],
  facets=["verify refuses", "warn is the default", "unreachable leaves it unverified"])
C('can-03', 'Constraint',
  "Under canary = warn, a canary check that passed within the last 10 minutes is trusted without embedding again; verify embeds at every writer attach.",
  CH, topic=T,
  q=["Does warn mode re-embed the canary phrases at every attach?"],
  rbd=[("Under canary = verify, a check that passed within the last 10 minutes is trusted without embedding again; warn embeds at every attach.", 'polarity', 'contradicts')],
  facets=["10 minutes", "verify embeds every attach"])
C('can-04', 'Logic',
  "[embedder] related_pairs (#87, #176): given a JSON file of at least 100 labelled related-but-distinct pairs, semantic_match_threshold becomes their p99 cosine under the configured embedder, provided that sits above the canary's random-pair floor; the result is cached against the file's SHA-256.",
  CH, topic=T,
  q=["How can the semantic merge threshold be derived from labelled data?",
     ("How many related pairs does lambo need before it derives a threshold?", 'fact-lookup'),
     ("MIN_RELATED_PAIRS", 'exact-identifier')],
  para="With a related_pairs file of 100 or more labelled related-but-distinct pairs, lambo sets semantic_match_threshold to the pairs' 99th-percentile cosine under the current embedder, as long as that is above the canary random-pair floor, and caches the value keyed by the file's SHA-256.",
  rbd=[("[embedder] related_pairs: with at least 50 labelled pairs, semantic_match_threshold becomes their median cosine.", 'quantity', 'contradicts'),
       ("A related_pairs file that fails to embed while the canary phrases succeed is retried with an exponential backoff (#191) instead of costing every attach a full re-embed.", 'topic-neighbour')],
  facets=["at least 100 pairs", "p99", "above the random-pair floor"])
C('can-05', 'Logic',
  "Under lambo's framing, unrelated pairs sit near 0.5 to 0.62 cosine on BGE-M3, so a merge threshold at the random-pair level would merge every related-but-distinct pair; the canary's random-pair quantiles are a relevance floor, never a merge threshold.",
  'src/embed/canary.rs', topic=T,
  q=["Why can't the canary's random-pair cosines be used as the merge threshold?"],
  facets=["0.5 to 0.62", "relevance floor only"])
NA(T, "Which twenty phrases make up the embedder canary?", note="the phrases live in source, not in this corpus")
NA(T, "How often has the canary caught a real drift on the dogfood store?")
NA(T, "What related-pairs threshold has been derived for EmbeddingGemma 2?")

T = 'calibration'
G = 'evidence/mooshik-g-recall-calibration/README.md'
C('cal-01', 'Observation',
  "G1 measured BGE-M3 on the llama.cpp rig: true recall (bare query to stored concept vector) 0.4599 to 0.6577; paraphrase merge pairs 0.6889 to 0.8984; related-but-distinct pairs 0.6348 to 0.8913; unrelated pairs 0.4999 to 0.6249.",
  G, topic=T,
  q=["What cosine range did G1 measure for paraphrase pairs under BGE-M3?",
     ("How much do BGE-M3 paraphrase and related-but-distinct cosines overlap?", 'vocabulary-mismatch')],
  para="G1's BGE-M3 measurements on the llama.cpp rig gave these cosine ranges: true recall 0.4599-0.6577, paraphrases 0.6889-0.8984, related but distinct 0.6348-0.8913, and unrelated 0.4999-0.6249.",
  rbd=[("G1 measured BGE-M3 paraphrase pairs at 0.80 to 0.95 and related-but-distinct pairs at 0.50 to 0.70, so the two classes separate cleanly at 0.75.", 'polarity', 'contradicts')],
  facets=["paraphrase 0.6889-0.8984", "related-distinct 0.6348-0.8913"])
C('cal-02', 'Constraint',
  "G2 kept semantic_match_threshold at 0.85 as a calibrated decision: 0.85 already accepts the related-but-distinct pair reset password / forgot password (0.8913), and lowering it to 0.84 would accept three more related-distinct pairs while rescuing only one paraphrase.",
  G, topic=T,
  q=["Why was the semantic merge threshold kept at 0.85?",
     ("Would lowering the merge threshold to 0.84 help?", 'contrast')],
  para="G2 left the merge threshold at 0.85. Dropping it to 0.84 would let three more related-but-distinct pairs merge and save only one paraphrase, and 0.85 already merges reset password with forgot password at 0.8913.",
  rbd=[("G2 lowered semantic_match_threshold from 0.85 to 0.80 so that BGE-M3 paraphrases like register user / create account would merge.", 'polarity', 'contradicts')],
  facets=["kept at 0.85", "0.84 adds three related-distinct pairs"])
C('cal-03', 'Constraint',
  "G2 set RECENT_SCORE to 0.35, below the lowest durable true-recall hit (0.3991), so max-merge keeps the correct vector ranking; the old 0.50 recent floor masked the 'changes that do not break existing clients' query.",
  G, topic=T,
  q=["What is RECENT_SCORE and why is it 0.35?",
     ("What did the old 0.50 recency floor break?", 'superseded'),
     ("RECENT_SCORE", 'exact-identifier')],
  para="RECENT_SCORE was lowered to 0.35 in G2, just under the weakest real durable recall hit at 0.3991, so a genuine vector match is never flattened by the recency floor; at 0.50 the floor had buried the right answer for the 'changes that do not break existing clients' query.",
  rbd=[("RECENT_SCORE is 0.50, a flat score given to every recent concept and above most true-recall cosines.", 'correction', 'supersedes'),
       ("In a session younger than 10 minutes, recency divides by a 10-minute floor, so a scheduler stall can no longer flip two scores.", 'topic-neighbour')],
  facets=["0.35", "below 0.3991"])
C('cal-04', 'Observation',
  "Under the 0.85 threshold, G records real paraphrases below it that create duplicates by design: register user / create account (0.8230), make an account / register user (0.8152), roll out the service / deploy service (0.8294).",
  G, topic=T,
  q=["Which paraphrase pairs fall below the 0.85 merge threshold?"],
  facets=["register user / create account 0.8230"])
NA(T, "What is the measured p99 of related-but-distinct pairs on the dogfood store?", note="this is what #190 has to produce")
NA(T, "How was the Gemini embedder calibrated against BGE-M3?")
NA(T, "How many paraphrase pairs did G1 measure under EmbeddingGemma 2?")
