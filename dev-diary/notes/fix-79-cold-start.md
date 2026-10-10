# Issue 79: recall cold start

The steady-state final score is `w_daemon × d + w_query × r`, where `d`
is the daemon score and `r` is phase-1 query relevance. Before the daemon
scores a newly derived concept, `d` is absent. With the default equal
weights, an EG2 relevant image at query similarity 0.7500 has a cold score
of 0.3750 and loses to observed older noise at 0.533.

## Chosen rule

When a real phase-1 concept backed by a keyword or vector leg has no ScoreTable entry,
score every expanded member by `w_query × r` for that recall. This includes
older scored members; giving only the new concept a query stand-in promotes
fresh irrelevant EG2 image similarity 0.6776 over an established relevant
image at 0.7500 if that older member has a daemon score below 0.6052. The
shared query scale instead orders them 0.3750 > 0.3388 at default weights.
The fresh relevant image at 0.7500 also beats older irrelevant similarity
near 0.61 (0.3750 > 0.3050). The same rule covers text and supplied image
vectors, without changing retrieval, merge, cosine, or RECENT_SCORE (#87).

Only query-backed phase-1 members may trigger this mode. A recent-only
member with the flat 0.35 score cannot trigger it; leg presence, not the
numeric value, decides this, so a vector similarity of exactly 0.35 still
counts as query evidence. Missing BFS members, siblings, and stale vector
IDs do not trigger it; an explicit daemon zero is present. Canonical
members retain their first partition, hot-list members retain force-inclusion,
and deterministic key/id ties remain. The query weight is retained as the
common multiplier rather than redistributing the daemon weight: this keeps
the configured score scale as far as possible while one component is absent.
If `w_query=0`, daemon-only recall remains daemon-only and cannot offer the
query-relevance cold-start guarantee. Positive custom query weights preserve
the same query ordering.

The phase-1 max merge gives keyword BM25, vector cosine, and recent-only
members one `r` value each. The cold rule scales every one by the same
`w_query`: at default weights a recent-only 0.35 becomes 0.175 and a
strong vector 0.75 becomes 0.375. A no-text recall-by-vector skips the
recent leg entirely. This is an ordering rule over the existing leg scores,
not a recalibration of them.

The next daemon score table restores the steady-state mix. A rank may change
at that transition, and the user explicitly accepted that limit. This rule
does not promise separation where an embedder itself orders irrelevant
similarity above relevant similarity.

## Measurements and rejected alternatives

The shipped EG2 reference set has relevant image similarity 0.7500–0.7943,
irrelevant image 0.5663–0.6776, relevant text 0.8228–0.8665, and irrelevant
text 0.5458–0.6656. The owned release live EG2 run on 2026-10-10 passed
three tests; mean relevant image/text similarities were 0.7667/0.8447.
The text-only no-projector branch returned early because it was not given a
second server.

On the committed session-rest-api graph (22 concepts, 12 interactions), the
source daemon formula yields min/median/max 0.1333/0.2470/0.6667. A new
Resource with one Derives edge and a thirteenth interaction has an immediate
intrinsic score about 0.2887 (recency 0.25 + activity 0.2/13 + density
0.35/15). These are source-and-fixture calculations, not a live daemon run.

| Missing-score approach | fresh image q=.7500 | fresh noise q=.6776 |
|---|---:|---:|
| zero daemon (old) | .3750 | .3388 |
| query stand-in only for new hit | .7500 | .6776 |
| fixture median d=.2470 | .4985 | .4623 |
| graph intrinsic d=.2887 | .5194 | .4832 |
| chosen, all hits use 0.5 query | .3750 | .3388 |

The chosen scores match the old fresh score numerically, but the older
scored competitors also lose their daemon contribution during the brief lag.
A median or on-demand graph intrinsic remains below the observed 0.533 old
noise, while query imputation only for the fresh hit violates the low-daemon
noise guard. Eager derive-time daemon scoring touches a wider write path and
does not address score-scale overlap. The chosen rule changes only the
cold-state ordering and leaves steady-state BGE-M3 goldens on their prior
formula. Their fixture bytes are unchanged. The pre-existing planted
assembly test intentionally has an unscored c2: its order changes from
`c1,c2,c5,c6,c4,c3` to `c1,c2,c5,c3,c4,c6`, because c3/c4/c6
lose their daemon contribution and tie at zero. Its scores change from
`.95,.375,.30,.15,.10,.05` in old order to `.75,.375,.15,0,0,0`
in new order. This is the only planned legacy expected-order change.

Baseline required test rows before edits (passed/ignored; zero failed):
SQLite+fixtures 1896/4; all+fixtures 1718/4; no-default SQLite 1035/0;
no-default Postgres 993/14; embed-eg2 1711/7; elastic+SQLite+fixtures
1980/4.

## Cadence and scope risk

The configured default daemon tick is one second, with write wakeups and
an epoch-gated rescore; one second is not a guaranteed score-publication
bound. An authorized read-only dogfood ledger copy held 1,045 derive and
1,098 recall calls over 51.47 days. Of 1,097 recalls following a derive,
6 (0.55%) occurred within one second of its call timestamp; among 342
recalls in multi-derive busy spans, 6 (1.75%) did. At 10 seconds those
proximity values are 3.74% and 9.94%; at 60 seconds, 17.05% and 40.94%.
These are temporal proxies, not observed cold-mode frequency or an
unconditional upper bound: the ledger lacks runtime score publication,
phase-1 provenance, and ScoreTable membership. Sustained derives or a
scoring backlog could make query-only recalls common. The mode ends when
all query-backed phase-1 candidates have ScoreTable entries; it has no
fixed wall-clock expiry. More cadence details are in the scratch report
`/tmp/lambo-79.XyH5Kv/79/cadence-review.md`.

## Existing graded-cosine test

`graded_similarity_ranks_by_cosine_not_recency_on_sqlite` sometimes ordered
its 0.3 look ahead of its 0.5 look because it asserted pure cosine order
while the test used a 50/50 daemon/query blend. The new cold rule cannot
fix that when all candidates are scored. The test now configures query-only
weights (0 daemon, 1 query), which is the invariant it actually tests;
the separate cold tests pin the missing-score behavior.
