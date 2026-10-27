"""More answerable queries (#173 follow-up, owner request 2026-10-27).

The v0 set had 12 vocabulary-mismatch queries (5 in dev), 8 rationale and 23
contrast. These are appended to existing concepts' query lists (so no
existing id or split moves), labelled on 2026-10-27, authored against the
corpus text only.

vocabulary-mismatch: the query asks for the concept's fact while avoiding
its distinctive words (identifiers, numbers, the names the text uses), as a
user who does not know them would. rationale: asks why. contrast: proposes a
nearby false claim the concept refutes.
"""
from common import Q

V, R, X = 'vocabulary-mismatch', 'rationale', 'contrast'

# ---- vocabulary-mismatch ------------------------------------------------
Q('emb-01', "In the first audit of the test machine's memory, what fraction of entries had no meaning-based index?", V, {'emb-02': 1})
Q('emb-03', "Did very long notes explain why so many stored notes could not be found by meaning?", V, {'emb-04': 1})
Q('emb-04', "Why did the local text-to-number server reject anything longer than a paragraph or so?", V, {'emb-03': 1})
Q('emb-06', "After the binary was rebuilt with the actions fix, how many old records got their missing numbers filled in?", V, {'emb-07': 1})
Q('emb-08', "Why did near-identical notes on the test machine pile up instead of being folded together?", V)
Q('cnd-02', "Which hosted model files does the built-in encoder download, and were they checked against the originals?", V)
Q('cnd-03', "What happens if the built-in encoder is started on a laptop without a graphics card?", V)
Q('cnd-05', "How did the team check that the in-process encoder produced the same numbers as the old server?", V)
Q('cnd-06', "What produces the meaning vectors on the Apple test machine now?", V, {'cnd-01': 1})
Q('can-01', "How does lambo notice that the model behind an unchanged settings name was quietly swapped?", V)
Q('can-03', "Is the startup self-test repeated on every attach when it only logs problems?", V)
Q('can-04', "Can the similarity cut-off for folding notes together be learned from examples a person marked?", V)
Q('cal-03', "Why does a note from the last few turns score lower than a weak meaning match?", V)
Q('cal-04', "Do some genuinely equivalent phrasings still end up stored twice, and which ones?", V)
Q('top-01', "How do the agents on the test machine all reach one shared memory process?", V)
Q('srv-01', "How long can the process holding a session go silent before another one may take over?", V, {'top-03': 1})
Q('srv-02', "Why could only one of several connected clients use the locking tool?", V)
Q('srv-07', "How long does a forwarding process keep a caller waiting before telling it to try again?", V)
Q('ack-04', "When a client is told its note was accepted, has the note actually been stored and indexed yet?", V, {'emb-05': 1})
Q('pro-04', "How was the second SQL database supported without maintaining two copies of the code?", V)
Q('pro-05', "Do the short names for the two SQL databases ever point at the other one?", V)
Q('prf-01', "How much faster did plain word search get, and how many requests a second could many clients make afterwards?", V)
Q('prf-04', "Did storing vectors more compactly in the single-file database lose precision?", V)
Q('rel-06', "What happens to an oversized request sent over standard input?", V)
Q('rel-07', "Can someone delete a whole project's memory, and can its name be taken again afterwards?", V)
Q('ses-01', "Can one running server keep separate memories per project and still share a common one underneath?", V)
Q('ses-02', "Why does a word match alone no longer pull in an entry from the shared memory?", V)
Q('con-03', "Does cleanup throw away the note that corrected an older mistaken note?", V)
Q('mg-01', "Why do two notes that differ only in a release number stay separate?", V)
Q('cn-01', "Why do many follow-up notes from a single exchange count only once toward promotion?", V)
Q('fnd-01', "Did agents use the memory tools just because the repository instructions told them to?", V)
Q('fnd-09', "Why did one coding client report that the memory server had no tools?", V)
Q('fnd-10', "When the process doing the writing died mid-request, what did the waiting client see?", V)
Q('g3-02', "Why does expanding from a single search hit so often add nothing?", V)
Q('eff-04', "When readers had the right material in front of them, how often did they answer correctly, and did they make things up?", V)
Q('eff-07', "Was the comparison between file notes and lambo fair in how much text each side got?", V)

# ---- rationale ----------------------------------------------------------
Q('can-05', "Why is the level unrelated pairs reach not a safe level for merging notes?", R)
Q('cal-02', "Why did G2 keep the merge threshold where it was instead of lowering it?", R)
Q('srv-01', "Why is the lease refreshed three times within one TTL?", R)
Q('srv-07', "Why does DIAL_BUDGET sit between the dial's own budgets and sqlite's busy_timeout?", R)
Q('pro-03', "Why was observability prioritised ahead of more implementation work?", R)
Q('pro-05', "Why is the postgres/cockroach alias split a loud failure rather than a silent one?", R)
Q('fnd-07', "Why can blast-radius warnings not fire on most orchestrator derives?", R)
Q('g3-03', "Why did the J3-implementation concept with 13 dependents not trigger a warning?", R)
Q('fnd-08', "Why was cross-client memory silently absent on default wiring in the two-product probe?", R)

# ---- contrast -----------------------------------------------------------
Q('cnd-04', "Does a proxying serve on Metal still load the full model?", X)
Q('can-03', "Does canary = verify trust a recent passing check like warn does?", X)
Q('cal-03', "Was the old 0.50 recent floor harmless for vector ranking?", X)
Q('srv-02', "Did one serve with several clients give each client its own lambo_reserve identity before J1?", X)
Q('ack-01', "Was the J3 derive ack slower than the FixtureEmbedder baseline?", X)
Q('rel-09', "Does [recall] kind = elastic move the source of truth to Elasticsearch?", X)
Q('ses-03', "With repo_scope, does README.md in two repositories become one shared hub concept?", X)
Q('con-05', "When the contradiction budget is full, are the oldest declared contradictions kept?", X)
Q('con-06', "Is canonization_supersede on by default?", X)
Q('mg-02', "Does canon alias-preview --similar merge the pairs it finds?", X)
Q('eff-05', "Did the file-memory arm present superseded facts as current?", X)
Q('fnd-11', "Does cursor-agent run lambo's MCP tools without extra flags?", X)
# mg-03 states the rule, not its reason, so it is asked as a contrast.
Q('mg-03', "Can lambo provision re-key older concepts while a session lease is live?", X)
