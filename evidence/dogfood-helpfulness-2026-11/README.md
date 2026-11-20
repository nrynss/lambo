# 92.5-day multi-agent dogfood usage and helpfulness audit (2026-08-19 to 2026-11-20)

This directory records a read-only empirical audit of 92.5 days of real daily multi-agent usage of Lambo across eight serving sessions and three coding-agent harnesses (`Claude Code`, `Codex`, `Cursor`), spanning eight shipped versions (`0.2.2` through `0.13.0`).

Unlike synthetic latency gates or offline retrieval benchmarks, this audit asks a practical question: **when real coding agents call Lambo on real repositories over three months, how often does recalled memory causally change plans or save discovery work — and where does it create noise, warning fatigue, or measurement error?**

> **Sanitization and privacy notice:** Following the precedent in [`evidence/README.md`](../README.md) and [`evidence/0.6.0-cutover/`](../0.6.0-cutover/README.md), this public report contains verified aggregates, public `lambo` repository artifacts (`#92`, `#172`, `#174`, `#175`, `#177`, `#181`, `#190`, `#198`, `#220`, `#248`, `#250`, `#251`, `#254`, `#256`, `#277`, `#279`, `#280`, and public `lambo` concept IDs), and anonymized behavioral case summaries. Non-`lambo` project names are anonymized as `project-a` through `project-e`; private concept IDs, internal ticket/PR identifiers, raw store content, and local filesystem or transcript paths are withheld.

## Artifacts in this directory

| File | What it contains |
|---|---|
| `README.md` | Full narrative report, methodology, case studies, documented friction modes, and caveats. |
| [`summary.json`](summary.json) | Machine-readable aggregate dataset backing every table in this report (zero private names, IDs, or paths). |

## Executive summary (three evidence layers)

| Evidence layer | Scope & snapshot | Headline measurements |
|---|---|---|
| **Level 1 — Server ledger** | `2026-08-19` to `2026-11-20` (`92.5` days, versions `0.2.2`–`0.13.0`, `0` dropped lines) | `5,248` MCP tool calls (`1,990` recalls, `1,923` derives, `1,144` `record_action`, `191` other); `324` caller-asserted `agent_id`s; `53,857` daemon heartbeats; `356` warning-bearing recalls; `74.0%` session-scoped `recall-first` (`52.2%` raw before `#279` fix). |
| **Level 2 — Live SQLite graph** | Read-only snapshot (`2026-11-20`) across `8` serving sessions | `7,594` concepts (`2,743` / `36.1%` in `lambo`; `4,851` / `63.9%` across `7` other sessions); `18,919` edges; `5` Canonical, `26` Venerable, `720` Candidate; `1,883` `Dependency` edges (`624` cross-agent, `250` >24h, `181` cross-agent & >24h, `165` >7d); `38` `Contradicts` edges (`13` cross-agent). |
| **Level 3 — Local transcripts & causal audit** | `260` Lambo-calling local sessions (`168` Claude Code, `64` Cursor, `28` Codex); `2,543` MCP calls; `657` recalls; stratified `n = 74` audit | **`45.9%`** Horvitz-Thompson population-weighted clearly helpful (**`50.0%`** unweighted `37/74`); `32.5%` (`31.1%`) confirmed/neutral; `21.6%` (`18.9%`) noise/ignored. **`50.8%`** (`225/443`) `0.6.0+` returned-ID citation rate (`50.3%` structured). **`53.8%`** (`14/26`) audited warnings ignored or false positive (`#280`). |

## 1. Causal helpfulness audit (`n = 74` of `N = 657` recalls)

Every recall in the `260` local sessions (`N = 657`) was assigned to one of three strata and sampled deterministically (`seed = 42`, guaranteeing coverage across smaller repositories):

1. **Warning-bearing recalls:** `26` sampled of `242`
2. **Cited-later non-warning recalls:** `20` sampled of `118`
3. **Baseline uncited non-warning recalls:** `28` sampled of `297`

Each sampled recall was graded by reading the surrounding agent turns before and after the tool response:

| Helpfulness tier | Unweighted sample (`n = 74`) | Horvitz-Thompson pop. estimate (`N = 657`) | Interpretation |
|---|---:|---:|---|
| **Changed plan / prevented mistake** | `18.9%` (`14/74`) | `17.4%` | Recalled ruling, prior review, or contradiction warning altered the agent's action or prevented duplicate/conflicting work. |
| **Saved discovery / guided work** | `31.1%` (`23/74`) | `28.5%` | Recalled runbook, baseline, or checklist directly guided execution without re-discovering context from scratch. |
| **Clearly helpful (combined)** | **`50.0%` (`37/74`)** | **`45.9%`** | Sum of `changed_plan` + `saved_discovery`. |
| **Confirmed existing context / neutral** | `31.1%` (`23/74`) | `32.5%` | Relevant memory confirmed context the agent already had, or cleared a pre-write gate without changing the plan. |
| **Noise / unhelpful / ignored** | `18.9%` (`14/74`) | `21.6%` | Off-topic cross-project hits, recency-floor backfill noise, or errored/discarded recalls. |

### Helpfulness by project group (`lambo` vs. non-`lambo`, unweighted)

| Project group | `n` | Changed plan | Saved discovery | Clearly helpful | Confirmed / neutral | Noise / ignored |
|---|---:|---:|---:|---:|---:|---:|
| **`lambo`** | `34` | **`29.4%`** | `23.5%` | `52.9%` | `26.5%` | `20.6%` |
| **Non-`lambo` (all other repos)** | `40` | `10.0%` | **`37.5%`** | `47.5%` | `35.0%` | `17.5%` |

Both halves show comparable overall helpfulness (`52.9%` vs. `47.5%`), but via different mechanisms: `lambo` carried denser design rulings and contradiction edges (`29.4%` `changed_plan`), whereas non-`lambo` repos got their largest payoff from operational memory such as deployment rollback runbooks, local test baselines, and PR verification checklists (`37.5%` `saved_discovery`).

### Helpfulness by harness (unweighted)

| Harness | `n` | Changed plan | Saved discovery | Clearly helpful | Confirmed / neutral | Noise / ignored |
|---|---:|---:|---:|---:|---:|---:|
| **Claude Code** | `29` | `17.2%` | `31.0%` | `48.3%` (`14/29`) | `20.7%` | `31.0%` |
| **Codex** | `20` | `25.0%` | `40.0%` | `65.0%` (`13/20`) | `35.0%` | `0.0%` |
| **Cursor** | `25` | `16.0%` | `24.0%` | `40.0%` (`10/25`) | `40.0%` | `20.0%` |

*See Caveat 1 below:* Codex local transcripts cover only `2026-11-03` to `2026-11-19` (entirely post-`0.6.0`, with isolated per-project sessions and 8-character IDs), whereas Claude Code and Cursor span the full three months including pre-split sessions.

### Warning audit (`n = 26`) and ledger warning breakdown

Across the `26` audited warning-bearing recalls:

| Audited warning outcome | Count (`n = 26`) | Share |
|---|---:|---:|
| **Acted on** (prompted inspection, verification, or plan change) | `7` | `26.9%` |
| **Noted only** (acknowledged in reasoning without changing behavior) | `5` | `19.2%` |
| **Ignored / false positive** (unrelated banner carried onto query) | `14` | **`53.8%`** |

Across the full 92.5-day server ledger, `356` of `1,990` recalls carried at least one warning banner (multi-label counts):

| Warning category (recall-level) | Recalls | Hit annotation type | Annotated hits |
|---|---:|---|---:|
| Hot warning | `205` | `hot` | `212` |
| Canonical marker | `64` | `load_bearing` | `140` |
| Blast-radius warning | `64` | `conflict` | `93` |
| Contradiction warning | `52` | `contradiction` | `62` |
| Conflict line | `44` | `contradicts` | `62` |
| Reservation warning | `13` | `rests_on_contradicted` | `23` |
| — | — | `reservation` | `13` |

Contradiction warnings drove several of the highest-value plan changes (`CS-01`, `CS-02`, `CS-07`), while unconditional Canonical-first banners attached to unrelated queries accounted for most warning fatigue (`CS-14`, tracked in `#280`).

## 2. Cross-harness behavioral scorecard (`260` local sessions)

Local transcripts across Claude Code (`168` sessions), Cursor (`64`), and Codex (`28`) capture `2,543` MCP calls and `657` recalls. On `0.6.0+` recalls displaying 8-character concept IDs (`443` recalls), **`50.8%` (`225/443`)** had at least one returned ID cited later in the session, and **`50.3%`** used structured citation fields (`depends_on`, `against`, or `lambo_inspect`).

| Harness | Local transcript window | Sessions | MCP calls | Recalls | Recall before write/edit | Recall before file edit | `0.6.0+` ID cited later | Structured cite | Warning recalls | Sample clearly helpful |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **Claude Code** | `2026-08-19` to `11-20` | `168` | `1,923` | `361` | `93.5%` (`116/124`) | `91.1%` (`90`) | `33.8%` (`79/234`) | `33.8%` | `131` | `48.3%` (`14/29`) |
| **Codex** | `2026-11-03` to `11-19` | `28` | `293` | `162` | `100.0%` (`27/27`) | `100.0%` (`11`) | `75.3%` (`122/162`) | `75.3%` | `87` | `65.0%` (`13/20`) |
| **Cursor** | `2026-08-24` to `11-20` | `64` | `327` | `134` | `77.0%` (`47/61`) | `68.2%` (`44`) | `51.1%` (`24/47`) | `46.8%` | `24` | `40.0%` (`10/25`) |
| **Total / overall** | `2026-08-19` to `11-20` | **`260`** | **`2,543`** | **`657`** | — | — | **`50.8%` (`225/443`)** | **`50.3%`** | **`242`** | **`50.0%` (`37/74`)** |

### Local transcript volume by project group (anonymized)

| Project group | Sessions | MCP calls | Recalls | Derives | Actions |
|---|---:|---:|---:|---:|---:|
| `project-a` | `166` | `1,488` | `302` | `686` | `493` |
| `lambo` | `48` | `813` | `265` | `300` | `191` |
| `project-b` | `25` | `139` | `52` | `29` | `54` |
| `other` (`general-usage` / misc) | `12` | `41` | `23` | `14` | `4` |
| `project-d` | `5` | `31` | `10` | `9` | `10` |
| `project-e` | `2` | `18` | `3` | `7` | `6` |
| `project-c` | `2` | `13` | `2` | `5` | `4` |
| **Total** | **`260`** | **`2,543`** | **`657`** | **`1,050`** | **`762`** |

## 3. Knowledge graph durability, cross-project footprint, and cross-model lineage

On `2026-11-20`, the live serving SQLite store held **`7,594` concepts** and **`18,919` edges** across **8 sessions**. Non-`lambo` sessions account for **`63.9%` (`4,851 / 7,594`)** of stored concepts and **`58.6%` (`1,104 / 1,883`)** of `Dependency` edges.

### All 8 serving sessions (anonymized)

| Session | Concepts | Canonical | Venerable | Candidate | None | Edges | Deps | Cross-agent deps | Deps >24h | Cross-agent & >24h | Deps >7d | Contradicts (cross-agent) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `lambo` | `2,743` | `5` | `15` | `444` | `2,279` | `7,299` | `779` | `271` | `184` | `138` | `129` | `6` (`2`) |
| `project-a` | `1,861` | `0` | `5` | `88` | `1,768` | `4,352` | `296` | `39` | `44` | `23` | `32` | `11` (`1`) |
| `project-b` | `1,012` | `0` | `0` | `59` | `953` | `2,516` | `391` | `133` | `13` | `13` | `0` | `0` (`0`) |
| `general-usage` | `910` | `0` | `0` | `35` | `875` | `2,390` | `238` | `180` | `8` | `6` | `3` | `19` (`10`) |
| `project-c` | `553` | `0` | `2` | `62` | `489` | `1,229` | `90` | `0` | `0` | `0` | `0` | `0` (`0`) |
| `project-d` | `411` | `0` | `3` | `30` | `378` | `909` | `76` | `1` | `1` | `1` | `1` | `2` (`0`) |
| `project-e` | `69` | `0` | `1` | `2` | `66` | `151` | `9` | `0` | `0` | `0` | `0` | `0` (`0`) |
| `general` | `35` | `0` | `0` | `0` | `35` | `73` | `4` | `0` | `0` | `0` | `0` | `0` (`0`) |
| **Total** | **`7,594`** | **`5`** | **`26`** | **`720`** | **`6,843`** | **`18,919`** | **`1,883`** | **`624`** | **`250`** | **`181`** | **`165`** | **`38` (`13`)** |

### Graph edge composition and `Dependency` survival funnel

| Edge type | Count | | `Dependency` durability filter | Count | Share of `1,883` |
|---|---:|---|---|---:|---:|
| `Derives` | `7,622` | | All `Dependency` edges | `1,883` | `100.0%` |
| `Causal` | `3,618` | | Cross-agent (distinct `agent_id`) | `624` | `33.1%` |
| `Temporal` | `3,103` | | Cross-day (>24h span) | `250` | `13.3%` |
| `CoOccurrence` | `2,537` | | Cross-agent **and** >24h | `181` | `9.6%` |
| `Dependency` | `1,883` | | Cross-week (>7d span) | `165` | `8.8%` |
| `Semantic` | `69` | | *(Plus `Contradicts` edges)* | `38` | `13` cross-agent (`34.2%`) |
| `Hierarchical` | `49` | | | | |
| `Contradicts` | `38` | | | | |

### Model-family citation handoff matrices

Rows are the citing model family; columns are the cited origin family. Off-diagonal cells count cross-family handoffs (`207 / 1,883` or `11.0%` overall; `79 / 779` or `10.1%` in `lambo`; `128 / 1,104` or `11.6%` in non-`lambo`).

#### All sessions (`1,883` `Dependency` edges · `207` cross-family / `11.0%`)

| Citing \ Cited | Claude | GPT / Codex | Gemini | Other | Row total |
|---|---:|---:|---:|---:|---:|
| **Claude** | `762` | `64` | `31` | `24` | `881` |
| **GPT / Codex** | `29` | `692` | `0` | `41` | `762` |
| **Gemini** | `4` | `0` | `17` | `1` | `22` |
| **Other** | `7` | `6` | `0` | `205` | `218` |

#### `lambo` only (`779` `Dependency` edges · `79` cross-family / `10.1%`)

| Citing \ Cited | Claude | GPT / Codex | Gemini | Other | Row total |
|---|---:|---:|---:|---:|---:|
| **Claude** | `329` | `60` | `0` | `10` | `399` |
| **GPT / Codex** | `4` | `346` | `0` | `0` | `350` |
| **Gemini** | `0` | `0` | `0` | `0` | `0` |
| **Other** | `3` | `2` | `0` | `25` | `30` |

#### Non-`lambo` sessions (`1,104` `Dependency` edges · `128` cross-family / `11.6%`)

| Citing \ Cited | Claude | GPT / Codex | Gemini | Other | Row total |
|---|---:|---:|---:|---:|---:|
| **Claude** | `433` | `4` | `31` | `14` | `482` |
| **GPT / Codex** | `25` | `346` | `0` | `41` | `412` |
| **Gemini** | `4` | `0` | `17` | `1` | `22` |
| **Other** | `4` | `4` | `0` | `180` | `188` |

### Selected public `lambo` pillar concepts

All `5` Canonical concepts on the rig live in `lambo`, joined by `15` Venerable concepts in `lambo` and `11` Venerable concepts across anonymized external sessions (`project-a`: `5`, `project-d`: `3`, `project-c`: `2`, `project-e`: `1`; private content withheld).

| Short ID | Status | Blast radius | Origin family | Created | Summary |
|---|---|---:|---|---|---|
| `ca2cb7d7` | Canonical | `10` | GPT / Codex | `2026-11-04` | Release tests are removed because full CI on the exact tagged commit is the test authority; release preflight must require both a successful CI check and matching tag. |
| `341fdd9c` | Canonical | `9` | GPT / Codex | `2026-10-15` | Review `#175` requires expansion relevance to reach assembly explicitly because `ExpandedSet` scores are discarded; G3 set Jaccard cannot estimate assembly gain. |
| `f586c6c0` | Canonical | `7` | GPT / Codex | `2026-10-30` | `#175` fresh graph-only challenge is baseline-screened and explicitly enriched, with direct-answer controls retained. |
| `db60cb86` | Canonical | `6` | GPT / Codex | `2026-10-30` | `#175` first experiment uses a crate-private assembly entry point with a separate graph-only relevance map; normal production callers remain untouched. |
| `b853d1aa` | Canonical | `6` | GPT / Codex | `2026-11-10` | Lambo real releases are triggered by pushing a matching `v<package-version>` tag, not `workflow_dispatch`; dispatch is diagnostic dry-run only. |
| `6c1285b7` | Venerable | `5` | GPT / Codex | `2026-10-30` | `#175` pilot freezes 80 authored answerable questions plus 8 absent-answer controls before running propagation, across 27 topic groups. |
| `6c9579da` | Venerable | `5` | Claude | `2026-10-10` | Lambo repo merge rule: after PR CI is green, read CodeRabbit's comment before merging. |

## 4. Ledger observability and `#177` offline drift replay

### Ledger volume by tool and session (`5,248` calls, `0` dropped lines)

| Tool | Calls | | Recorded ledger session | Total | Recalls | Derives | Actions | Warning recalls | Unique IDs |
|---|---:|---|---|---:|---:|---:|---:|---:|---:|
| `lambo_recall` | `1,990` | | `pre-split` (unspecified) | `2,994` | `1,149` | `1,097` | `647` | `123` | `178` |
| `lambo_derive` | `1,923` | | `general-usage` | `623` | `231` | `212` | `165` | `79` | `77` |
| `lambo_record_action` | `1,144` | | `lambo-dev` | `563` | `202` | `220` | `92` | `8` | `36` |
| `lambo_stats` | `130` | | `lambo` | `563` | `304` | `165` | `68` | `113` | `50` |
| `lambo_reserve` | `41` | | `project-a` | `480` | `95` | `218` | `167` | `31` | `5` |
| `lambo_inspect` | `17` | | `project-d` | `25` | `9` | `11` | `5` | `2` | `2` |
| `lambo_saints` | `2` | | **Total** | **`5,248`** | **`1,990`** | **`1,923`** | **`1,144`** | **`356`** | **`324`** *(distinct)* |
| `lambo_stale` | `1` | | | | | | | | |

Weekly volume peaked in `2026-W41` (`1,432` calls: `594` recalls, `556` derives, `243` actions) around the `0.6.0` multi-session cutover (`2026-W43` recorded zero calls; full weekly series in [`summary.json`](summary.json)).

### `recall_first.py` bug (`#279`): `52.2%` raw vs. `74.0%` session-scoped

Running `scripts/observability/recall_first.py` verbatim over the 92.5-day ledger reported **`52.2%`** (`922 / 1,765` write sequences) because `scripts/observability/_ledger.py`'s `restart_times()` compares consecutive `uptime_secs` values globally across interleaved heartbeats from concurrent per-session daemons in `calls.jsonl`. That produced **`38,389` false restart splits**.

Keying restart detection by session drops detected restarts to **`97` true restarts** and lifts ledger-wide compliance to **`74.0%` (`1,083 / 1,464`)** (tracked in `#279`):

| Model family | Ledger calls | Unique IDs | Raw `recall_first.py` | Session-scoped adjusted | Adjusted derives w/o prior recall |
|---|---:|---:|---:|---:|---:|
| **Claude** | `2,501` | `71` | `31.7%` (`312/983`) | `56.1%` (`410/731`) | `383` |
| **GPT / Codex** | `2,135` | `99` | `81.6%` (`493/604`) | `96.8%` (`539/557`) | `25` |
| **Gemini** | `99` | `33` | `9.7%` (`3/31`) | `58.6%` (`17/29`) | `5` |
| **Other** | `513` | `121` | `77.6%` (`114/147`) | `79.6%` (`117/147`) | `23` |
| **All families** | **`5,248`** | **`324`** | **`52.2%` (`922/1,765`)** | **`74.0%` (`1,083/1,464`)** | **`436`** |

### `dedup_rate.py` and `score_bands.py`

- **`dedup_rate.py` (`1,920` ok derives; `3` failed, `1` factless):** Exact match rate **`2.24%`** (`81` exact matches); effective convergence **`2.72%`** (`18` semantic merges and `6` reinforcements; both counters were absent on `1,026` pre-`0.4.0` derives). Derives created `3,542` concepts and `lambo_record_action` created `3,986` additional concepts.
- **`score_bands.py` (`1,989` recalls; `1,690` used vectors; `0` zero-hit recalls; `229` phase-2 expansion hits):**
  - The `0.35` recency leg appeared on **`18.5%`** (`2,282 / 12,323`) of phase-1 hits, with **`1.3%`** (`162`) recency-floor-only and **`0`** vector hits masked below `0.35` (G1 drift mean delta `+0.0503`).

| Retrieval signal | Hits (`n`) | p10 | Median | p90 | Min–Max |
|---|---:|---:|---:|---:|---:|
| **BM25 (keyword)** | `12,065` | `6.60` | `15.00` | `27.16` | `0.43` to `92.87` |
| **Vector cosine** | `4,540` | `0.549` | `0.635` | `0.711` | `0.305` to `1.000` |
| **Fused score** | `12,552` | n/a | `7.50` | `13.62` | `0.06` to `46.54` |

### `#177` offline drift replay (`scripts/loadtest/ledger_drift.py`)

Replaying the full 92.5-day ledger through `scripts/loadtest/ledger_drift.py` evaluated **`46` of `61` streams** (`0` dropped/unparseable lines) and raised **`11` alarms**. The strongest alarm (`peak log10(M) = 24.64`) caught the `2026-09-07` recall `hit_count` tightening (`25.0 -> 5.0` median):

| Alarm date | Session | Contract | Query class | Stream | Shift | Before -> After median | Peak `log10(M)` | `n` | Nearby events |
|---|---|---|---|---|---|---:|---:|---:|---|
| `2026-09-07` | `default` | `unknown (pre-0.6.0)` | `hybrid` | `hit_count` | `low` | `25.0 -> 5.0` | `24.64` | `967` | None |
| `2026-10-05` | `default` | `unknown (pre-0.6.0)` | `hybrid` | `top_bm25_legacy` | `high` | `15.73 -> 25.75` | `8.13` | `967` | None |
| `2026-10-05` | `default` | `unknown (pre-0.6.0)` | `hybrid` | `top_fused` | `high` | `8.04 -> 13.03` | `7.84` | `967` | None |
| `2026-10-05` | `default` | `unknown (pre-0.6.0)` | `keyword_only` | `hit_count` | `low` | `5.0 -> 5.0` | `4.39` | `181` | None |
| `2026-09-05` | `default` | `unknown (pre-0.6.0)` | `hybrid` | `top_vector_cosine` | `high` | `0.599 -> 0.672` | `4.05` | `967` | None |
| `2026-09-11` | `default` | `unknown (pre-0.6.0)` | `keyword_only` | `top_bm25_legacy` | `high` | `10.69 -> 16.68` | `3.67` | `181` | None |
| `2026-10-12` | `lambo-dev` | `candle/bge-m3/1024` | `hybrid` | `top_fused` | `high` | `9.90 -> 13.43` | `3.28` | `182` | None |
| `2026-10-05` | `default` | `unknown (pre-0.6.0)` | `keyword_only` | `top_fused` | `high` | `6.70 -> 9.22` | `3.09` | `181` | None |
| `2026-10-13` | `lambo-dev` | `candle/bge-m3/1024` | `hybrid` | `top_bm25_legacy` | `high` | `19.51 -> 26.55` | `3.03` | `182` | `2 uptime_gap restarts` |
| `2026-11-03` | `general-usage` | `candle/bge-m3/1024` | `hybrid` | `top_bm25_legacy` | `high` | `10.22 -> 16.42` | `2.52` | `159` | None |
| `2026-11-03` | `general-usage` | `candle/bge-m3/1024` | `hybrid` | `top_fused` | `high` | `5.51 -> 7.68` | `2.37` | `191` | None |

## 5. Representative case studies (public `lambo` and anonymized external)

### High-value causal saves

| Case | Scope & harness | Date | Tier | What Lambo recalled & what the agent did next |
|---|---|---|---|---|
| **CS-02** | `lambo` · Cursor | `2026-11-19` | `changed_plan` *(contradiction)* | Triage across `0.13.0` open issues surfaced `#254` layer-floor decisions alongside a live contradiction warning. The agent immediately called `lambo_saints`, `lambo_stale`, and `lambo_inspect` to verify settled constraints before selecting `#248`, `#250`, and `#177`. |
| **CS-03** | `lambo` · Codex | `2026-11-12` | `changed_plan` *(canonical+load_bearing)* | `#175` retirement query recalled Canonical rulings `341fdd9c`, `db60cb86`, and `f586c6c0`. The agent archived all eight `#175` reports under `research/issue-175/` as "evaluated, not adopted", preserved aggregate JSON unchanged, updated `#172`/`#220`, and cited those Canonical IDs in `lambo_derive` (`depends_on`). |
| **CS-04** | `lambo` · Codex | `2026-11-12` | `changed_plan` *(canonical+contradiction)* | `#177` planning recall surfaced `#175` baseline findings and `#190` prerequisites. The agent recommended `#177` offline ledger drift next (deferring `#174` until `#190` collects no-answer data) and filed a structured `against` contradiction replacing fixed-reference CUSUM with online randomized p-values. |
| **CS-06** | `lambo` · Claude Code | `2026-10-27` | `changed_plan` | `#198` recall surfaced exact prior ruling `f00d7711` (masked-Jaccard pair matching). The agent updated `#198` to adopt one-span + value-slot matching and recorded a structured `against` supersession targeting `f00d7711`. |
| **CS-08** | `lambo` · Cursor | `2026-10-10` | `changed_plan` | `#92` portal CSP query recalled earlier review recommendation `6dcffb4c` proposing a `<meta http-equiv>` CSP tag. The agent reconciled it against the owner's override and kept CSP and `nosniff` as HTTP response headers only. |
| **CS-01** | External docs repo (`project-a`) · Claude Code | `2026-11-19` | `changed_plan` *(contradiction)* | Contradiction warning against an older staging check flagged a two-setting prerequisite change for PR-comment dismissal. The agent halted and verified the existing recommended-setup docs before drafting conflicting edits. |
| **CS-05** | External TypeScript/Svelte repo · Codex | `2026-11-03` | `changed_plan` | Recalled an architectural invariant requiring reactive attach bindings to run inside `untrack()` to avoid self-subscription cycles. The agent applied the invariant, cited four returned concept IDs, and added a regression test. |
| **CS-07** | External CI / ops repo · Claude Code | `2026-11-12` | `changed_plan` *(contradiction+hot)* | Contradiction warning on Slack alert deduplication prompted empirical verification of live CI posts instead of rotating a working CI credential — proving only a local token had expired. |
| **CS-09** | External infrastructure repo (`project-a`) · Claude Code | `2026-11-13` | `saved_discovery` | Recovered a two-month-old container backup-and-swap rollback runbook (tarball backup, pause, rename stopped old container for instant rollback). The agent executed the exact sequence and cited the runbook concept in `lambo_record_action`. |
| **CS-10** | External Go/UI repo (`project-a`) · Cursor | `2026-11-13` | `saved_discovery` | Reviewer-to-remediator handoff across separate Cursor sessions recovered prior PR review findings and worktree context, scoping fixes cleanly without touching excluded identities. |
| **CS-11** | External API repo (`project-d`) · Cursor | `2026-10-13` | `saved_discovery` | Recovered three exact prior PR review findings (deprecation grammar, request-body attribution, pagination sort) as a verification checklist without re-reading the full PR thread. |
| **CS-12** | External TypeScript/Svelte repo · Codex | `2026-11-03` | `saved_discovery` | Recovered a recorded local Playwright baseline (`459` passing, `16` skipped, `26` pre-existing host failures), immediately separating pre-existing host flakes from PR regressions. |

### Documented failure and friction modes (`FM-1`–`FM-4`)

Four concrete mechanisms explain nearly all audited noise, warning fatigue, and metric distortion:

| Mode | Severity | Measured impact | Mechanism & case evidence | Follow-up issue(s) |
|---|---|---|---|---|
| **FM-1: Canonical-first warning carryover** | High | `53.8%` (`14/26`) audited warnings ignored or false positive | `src/recall/assemble.rs` injects high-blast-radius Canonical nodes and their load-bearing/conflict banners onto unrelated queries. In **CS-14** (`lambo`, Codex), a `#250`/`#251` code-review query inherited Canonical release-test ruling `ca2cb7d7` with load-bearing and conflict warnings; the agent cited the two relevant hits and skipped the unrelated banner. | `#280` — relevance-gate Canonical-first injection and warning banners. |
| **FM-2: `general-usage` bleed + `0.35` recency-floor backfill** | High | `18.5%` (`2,282/12,323`) phase-1 hits carry `0.35` leg; `1.3%` (`162`) recency-floor-only | Routing repo-specific queries into shared `general-usage` (or pre-split `default`) lets the `0.35` recency leg backfill sparse queries with unrelated fresh notes. In **CS-15** (`lambo`, Cursor), a `0.8.0` roadmap query returned three unrelated external-project notes at `0.35`, which the agent rejected. In **CS-16** (sibling Rust repo `project-c`, Claude Code, pre-split), the single shared graph leaked another repo's issue roadmap and was discarded. | `#174` (conformal abstention), `#254` (layer-floor size effects), `#277` (isolated per-project setup). |
| **FM-3: Write-gate performative recall & headless denials** | Medium | `33` errored headless Claude Code recalls; late recall in write-gated reviews | Mid-write hooks push agents to call `lambo_recall` just to clear a gate after a review is already drafted (**CS-13**, external web repo `project-b`, Cursor: *"The gate should now be lifted. Let me retry writing the review file"*). Non-interactive Claude Code runs also hit MCP tool permission denials (`33` errored recalls). | `#277` — shift recall nudges to task/session start and pre-allow read-only `lambo_recall` in headless profiles. |
| **FM-4: Multi-session heartbeat bug in `recall_first.py`** | Medium | `38,389` false restart splits (`52.2%` raw vs. `74.0%` true compliance) | `_ledger.restart_times()` in `scripts/observability/_ledger.py` compares consecutive `uptime_secs` values globally across concurrent per-session daemons in `calls.jsonl`. Keying restarts per session drops restarts from `38,389` to `97` and matches the per-stream boundary semantics in `scripts/loadtest/ledger_drift.py`. | `#279` — scope `restart_times()` by session in `_ledger.py`. |

## 6. Methodology caveats and evidence limits

1. **Asymmetric transcript retention windows by harness:** Local Codex transcripts span only `2026-11-03` to `2026-11-19` (entirely post-`0.6.0`, with isolated per-project sessions, displayed 8-character IDs, and hooks), whereas Claude Code (`2026-08-19` to `2026-11-20`) and Cursor (`2026-08-24` to `2026-11-20`) span the full three months including early pre-split and pre-`0.6.0` sessions without displayed concept IDs. Cross-harness percentages reflect both harness behavior and era mix.
2. **Three distinct `recall-first` measurement layers:** Raw `recall_first.py` reports `52.2%` (`922/1,765`) because of `38,389` false multi-daemon restart splits (`#279`); session-scoped recalculation yields `74.0%` (`1,083/1,464`, `97` true restarts); local transcript recall-before-write/edit is higher still (`77.0%`–`100.0%`) because interactive hooks enforce recall-first on newer sessions.
3. **Stratification oversampling vs. Horvitz-Thompson population estimates:** The `n = 74` qualitative audit intentionally oversampled warning-bearing recalls (`26/242`) and cited-later non-warning recalls (`20/118`) alongside `28/297` baseline uncited recalls (`seed = 42`, ensuring minor-repo representation). Unweighted helpfulness (`50.0%`) is therefore higher than the Horvitz-Thompson population estimate (`45.9%`); both are reported side by side.
4. **Cursor tool-result reconstruction and conservative silent-save undercount:** Cursor local transcripts omit raw MCP `tool_result` payloads, so `0.6.0+` returned concept IDs (`2026-10-13` onward) and warning flags were joined back to the server ledger via exact `(agent_id, query, ±3d)` matches (`132/134` matched). Silent saves where an agent steers without quoting or citing a recalled ID remain a conservative undercount.
5. **Caller-asserted `agent_id` fragmentation:** `324` unique caller-asserted `agent_id`s appear across `5,248` ledger calls, including task-scoped IDs. This routes some single-model activity into `Other` (`513` calls, `121` IDs) and inflates cross-agent `Dependency` counts when one model family rotates task-scoped IDs.
