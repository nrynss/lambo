-- Method for evidence/layer-floor-size. DuckDB. Session names are placeholders:
-- replace :query_session and the :unrelated_sessions list with sessions of your own store.
-- Inputs: vectors.jsonl from export_vecs.py (one row per concept: id, store, session, ctype,
-- created_at, v), queries.jsonl (one row per logged recall: qid, session, ts, top[{id,cos}], v,
-- where v is the query text embedded through embed_queries.py), origin.jsonl (id, origin = the
-- concept's origin_interaction).

create table c as select id, store, session, ctype, created_at::timestamptz created_at, v::FLOAT[1024] v
  from read_json('vectors.jsonl', maximum_object_size=200000000);
create table q as select qid, session, ts::timestamptz ts, top, v::FLOAT[1024] v
  from read_json('queries.jsonl', maximum_object_size=200000000);

-- 1. Query-vector check: recomputed cosine against the cosine the server logged for the same pair.
with t as (select unnest(q.top) h, q.v from q),
     d as (select h.cos logged, array_cosine_similarity(t.v, c.v) mine from t join c on c.id = h.id)
select count(*) pairs, avg(mine - logged) mean_signed_diff, corr(mine, logged) correlation,
       regr_intercept(logged, mine) intercept, regr_slope(logged, mine) slope from d;

-- 2. Stand-in layer: concepts of sessions unrelated to the query session, in three random orders.
create table pool as
  select id, v,
         row_number() over (order by hash(id || 'p1')) r1,
         row_number() over (order by hash(id || 'p2')) r2,
         row_number() over (order by hash(id || 'p3')) r3
  from c where session in (:unrelated_sessions);

-- 3. Best cosine of each query over the first N pool concepts, for N = 8 .. 4096 and each order.
--    corrected = intercept + slope * raw, from step 1. A query "admits" at floor f when best >= f.
--    Shown for one N and one order; the run repeats it per N and order.
select avg((best >= 0.50)::int) share_admitting
from (select q.qid, max(:intercept + :slope * array_cosine_similarity(q.v, p.v)) best
      from q join pool p on p.r1 <= :n where q.session = :query_session group by all);

-- 4. Merge side. For each fact-type concept, the best cosine to a concept of an EARLIER call
--    (arrival order), and to concepts of other calls in a random subset of the session.
create table cr as
  select c.*, o.origin,
         row_number() over (partition by session order by created_at, id) arrival
  from c join read_json('origin.jsonl') o using (id);
--    LEFT join: a fact with no earlier concept from another call keeps a row with a NULL
--    best_earlier. It counts as a miss in the share (coalesce to 0). The published
--    percentiles also take it as 0; that is 18 of the 4,434 facts.
create table best as
select a.id, a.session, a.arrival, max(array_cosine_similarity(a.v, b.v)) best_earlier
from cr a left join cr b on a.session = b.session and b.arrival < a.arrival and a.origin <> b.origin
where a.ctype in ('Logic', 'Observation', 'Constraint') group by all;
select session, count(*) facts, avg((coalesce(best_earlier, 0) >= 0.85)::int) share_ge_085
from best group by all;
