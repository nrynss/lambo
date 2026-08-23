# B4 implementation

Orchestrator-authored after the B4 implementor died on a 402 before
writing anything. Branch `b0-pg-extraction`. Not committed.

## What shipped

Postgres live-schema width is probed at `init_schema` and
`preflight_schema` via `Dialect::live_schema_vector_width_sql`. Cockroach
returns `None` (static file remains the authority). The probe is
`format_type(atttypid, atttypmod)` on `concepts.embedding`, parsed as
`vector(n)` only (`VECTOR(n)` is rejected).

If live n disagrees with construction dim, attach fails naming both
numbers. `GraphStore::vector_dimensions` still returns the construction
width; after a successful probe that number is the schema's.

## Tests

- `parse_pgvector_format_type_reads_vector_n` (unit)
- `live_schema_width_refuses_a_config_that_disagrees` (`#[ignore]`, live):
  init at 768, construct 1536 against the same DSN, `preflight_schema`
  errors. Goes red if the live check is skipped.
- Two-width init already reads live `format_type`; it now also asserts
  `vector_dimensions() == Some(dim)`.

CI `postgres-live` first step greps the new mismatch test.

## Not B4

Pin check at `resolve_backends` is unchanged. B3 ranking unchanged.
sqlite.rs untouched.
