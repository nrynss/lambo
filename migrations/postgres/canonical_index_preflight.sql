-- Keep the replacement definition in sync with 001_init.sql's atomic guard.
SELECT EXISTS (
    SELECT 1 FROM pg_index i
    JOIN pg_class idx ON idx.oid = i.indexrelid
    JOIN pg_am am ON am.oid = idx.relam
    WHERE i.indexrelid = to_regclass('concepts_key_digest_non_obs_idx')
      AND i.indrelid = 'concepts'::regclass
      AND i.indisunique AND i.indisvalid AND i.indisready AND i.indislive
      AND i.indnkeyatts = 2 AND i.indnatts = 2 AND am.amname = 'btree'
      AND pg_get_indexdef(i.indexrelid, 1, true) = 'session_id'
      AND pg_get_indexdef(i.indexrelid, 2, true) = 'lambo_canonical_key_digest(canonical_key)'
      AND pg_get_expr(i.indpred, i.indrelid) = '(concept_type <> ''Observation''::text)'
) AND NOT EXISTS (
    -- Covers both the old partial index and a table-level UNIQUE constraint,
    -- even if an operator renamed either one.
    SELECT 1 FROM pg_index i
    WHERE i.indrelid = 'concepts'::regclass AND i.indisunique
      AND i.indnkeyatts = 2
      AND pg_get_indexdef(i.indexrelid, 1, true) = 'session_id'
      AND pg_get_indexdef(i.indexrelid, 2, true) = 'canonical_key'
);
