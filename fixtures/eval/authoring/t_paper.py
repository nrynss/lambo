from common import C, NA, P
PR = 'nrynss/lambo-paper:evaluation/results/2026-09-07/README.md'
CH = 'CHANGELOG.md'
T = 'efficacy'
C('eff-01', 'Observation',
  "Paired memory comparison 2026-09-07, lambo project run: Claude Code file memory scored 8/12 and the Lambo arm 2/12, with a reader on claude-sonnet-5 and a blind Opus grader.",
  PR, topic=T,
  q=["How did Lambo recall compare with file memory on the lambo project in the 2026-09-07 comparison?",
     ("What did the Lambo arm score in the lambo-project memory comparison?", 'fact-lookup')],
  para="In the 2026-09-07 paired comparison on the lambo project, file memory got 8 of 12 graded answers right and the Lambo arm 2 of 12; answers were read by claude-sonnet-5 and graded blind by Opus.",
  rbd=[("Paired memory comparison 2026-09-07, vimanam project run: file memory scored 10/12 and the Lambo arm 8/12.", 'repository', 'distinct_entity'),
       ("Paired memory comparison 2026-09-07, lambo project run: the Lambo arm scored 8/12 and file memory 2/12.", 'polarity', 'contradicts')],
  facets=["file 8/12", "Lambo 2/12"])
C('eff-02', 'Observation',
  "Paired memory comparison 2026-09-07, vimanam project run: file memory 10/12, Lambo 8/12; each system won exactly the questions only it had ingested, and the two June 2026 decisions predate the store.",
  PR, topic=T,
  q=["Why did Lambo lose the packaging and brand questions in the vimanam run?",
     ("What did each arm score in the vimanam run?", 'fact-lookup', {'eff-01': 1})],
  para="On the vimanam project the file arm scored 10 of 12 and Lambo 8 of 12. Each side won exactly the questions only it had ingested: Lambo the in-session implementation decision that never reached a note, file memory the two June 2026 decisions older than the store.",
  facets=["file 10/12, Lambo 8/12", "each won what only it had ingested"])
C('eff-03', 'Observation',
  "Over the seven questions with an answer-bearing concept in the 990-concept store copy: covered@1 was 2, @2 through @10 was 4, and @20 was 6; first answer-bearing ranks were 1, 1, 2, 2, 3, 7 and beyond 20. The rendered context held only two or three hits.",
  PR, topic=T,
  q=["What was retrieval coverage by depth in the 2026-09-07 efficacy readout?",
     ("Why did readers effectively see only four of seven answers?", 'multi-hop', {'eff-04': 1})],
  para="Of the seven questions whose answer existed in the 990-concept store copy, 2 were covered at rank 1, 4 within ranks 2 to 10, and 6 within 20; the first answer-bearing hit came at ranks 1, 1, 2, 2, 3, 7 and past 20, while the rendered recall text carried just two or three hits.",
  rbd=[("Over the seven answerable questions, covered@1 was 4 and covered@20 was 7; every answer-bearing concept ranked in the top ten.", 'quantity', 'contradicts')],
  facets=["covered@1 2", "@20 6", "context held 2-3 hits"])
C('eff-04', 'Observation',
  "Efficacy readout: readers were correct on 6 of 8 answers where the rendered context covered the rubric and 0 of 6 where it did not; 0 of 48 answers invented a fact, and abstention was the failure mode.",
  PR, topic=T,
  q=["Did the reader hallucinate when Lambo's context lacked the answer?",
     ("How accurate were readers when the recall context covered the rubric?", 'fact-lookup')],
  para="When the recall text covered the rubric, readers answered 6 of 8 correctly; when it did not, 0 of 6. None of the 48 answers made up a fact; failing answers abstained instead.",
  rbd=[("Efficacy readout: in the lambo run's Lambo arm, 2 of 12 answers presented superseded facts as current and 2 of 12 repeated a stale hypothesis.", 'topic-neighbour')],
  facets=["6 of 8 covered", "0 of 6 uncovered", "0 of 48 invented"])
C('eff-05', 'Observation',
  "Efficacy readout, staleness: in the lambo run's Lambo arm, 2 of 12 answers presented superseded facts as current (the embedder question) and 2 of 12 repeated a stale hypothesis; none did in the vimanam run or in any of the 24 file-arm answers.",
  PR, topic=T,
  q=[("How often did Lambo-arm answers present superseded facts as current?", 'superseded', {'con-01': 1}),
     ("Which question came back stale in the Lambo arm?", 'fact-lookup', {'cnd-06': 1})],
  para="Staleness in the readout: two of the twelve Lambo-arm answers in the lambo run gave superseded facts as current, both on the embedder question, and two repeated a stale hypothesis, while the vimanam run and all 24 file-arm answers had none.",
  facets=["2 of 12 superseded", "embedder question", "file arm 0"])
C('eff-06', 'Observation',
  "Efficacy readout, efficiency: evidence tokens per correct answer were 2,467 for file against 477 for Lambo in the vimanam run, and 4,542 against 2,058 in the lambo run; server-side recall p50 was 86 to 94 ms at 990 concepts.",
  PR, topic=T,
  q=["How many evidence tokens per correct answer did Lambo use compared with file memory?"],
  para="Per correct answer, the vimanam run used 2,467 evidence tokens for file memory versus 477 for Lambo, and the lambo run 4,542 versus 2,058; recall took 86-94 ms at p50 on the server with 990 concepts.",
  rbd=[("Efficacy readout: full prompt tokens per correct answer were 4,334 for file against 2,994 for Lambo in the vimanam run, and 6,628 against 12,270 in the lambo run.", 'topic-neighbour')],
  facets=["2,467 vs 477", "4,542 vs 2,058"])
C('eff-07', 'Constraint',
  "Caveat on the 2026-09-07 comparison: the file arm received the whole memory directory (8,222 or 12,113 bytes) while the Lambo arm received only the rendered recall text, 1,154 to 1,533 bytes holding two or three of five or six hits.",
  PR, topic=T,
  q=["Was the 2026-09-07 comparison budget-matched between the arms?",
     ("How many bytes of context did the Lambo arm get?", 'fact-lookup')],
  para="The comparison was not budget-matched: file memory got its entire directory, 8,222 or 12,113 bytes, while Lambo got only the rendered recall text of 1,154-1,533 bytes, carrying two or three of its five or six hits.",
  rbd=[("Controls not run: a matched-corpus run that ingests the same notes into a fresh Lambo session, and an equal-budget run that caps the file arm at the Lambo byte budget.", 'topic-neighbour')],
  facets=["file arm whole directory", "Lambo 1,154-1,533 bytes"])
NA(T, "Under which GitHub organization does vimanam ship its distribution, and why was the personal handle not used?", family='efficacy',
   note="efficacy question brand-org (vimanam run); the vimanam project is outside this corpus")
NA(T, "Why did CI fail on vimanam pull requests #91 and #92 on 2026-09-04, and what was done about it?", family='efficacy', note="efficacy question ci-clippy-failure (vimanam run)")
NA(T, "How does vimanam's diff subcommand (#45) compare two spec versions, and which design option was chosen?", family='efficacy', note="efficacy question diff-design (vimanam run)")
NA(T, "How are pull requests merged into vimanam's main branch, and why is that path used?", family='efficacy', note="efficacy question merge-workflow (vimanam run)")
NA(T, "How many downloads has vimanam had to date on crates.io and through its Homebrew tap?", family='efficacy', note="efficacy question missing-downloads (vimanam run): absent by design")
NA(T, "Which release-automation tool did vimanam adopt to resolve issue #32, and which alternative was rejected?", family='efficacy', note="efficacy question packaging-decision (vimanam run)")
NA(T, "What did the matched-corpus control run of the memory comparison show?")
NA(T, "What was the inter-grader agreement between the Opus grader and a human?")
NA(T, "How did Lambo score against file memory with a GPT reader?")

T = 'credentials'
C('gcp-01', 'Constraint',
  "0.7.0 (#7): with neither GCP_LAMBO_CREDENTIALS nor GOOGLE_APPLICATION_CREDENTIALS set, Gemini and Cloud SQL IAM use the file gcloud auth application-default login writes: $CLOUDSDK_CONFIG/application_default_credentials.json when CLOUDSDK_CONFIG is set, else ~/.config/gcloud/application_default_credentials.json.",
  CH, topic=T,
  q=["Do I still need to export GOOGLE_APPLICATION_CREDENTIALS after gcloud login?",
     ("Where does lambo look for gcloud application-default credentials?", 'fact-lookup'),
     ("GCP_LAMBO_CREDENTIALS", 'exact-identifier')],
  para="Since 0.7.0 (#7), if neither GCP_LAMBO_CREDENTIALS nor GOOGLE_APPLICATION_CREDENTIALS is set, Gemini and Cloud SQL IAM fall back to the credentials file written by gcloud auth application-default login, at $CLOUDSDK_CONFIG/application_default_credentials.json if CLOUDSDK_CONFIG is set and ~/.config/gcloud/application_default_credentials.json otherwise.",
  rbd=[("A credential variable naming a missing file fails loudly and names the path; the gcloud default file is used only if it exists and is a file.", 'exception', 'distinct_entity'),
       ("0.7.0 (#7): Gemini reads only GOOGLE_APPLICATION_CREDENTIALS; without it, it refuses to start even after gcloud login.", 'polarity', 'contradicts')],
  facets=["neither variable set", "application_default_credentials.json", "CLOUDSDK_CONFIG"])
C('gcp-02', 'Constraint',
  "CLOUDSDK_CONFIG joins RESOLVE_ENV_VARS; clearing it falls back to the gcloud directory under HOME, so a hermetic test harness should point it at an empty directory.",
  CH, topic=T,
  q=["How should a hermetic test harness handle gcloud credentials?"],
  para="CLOUDSDK_CONFIG is now part of RESOLVE_ENV_VARS, and unsetting it falls back to the gcloud directory under HOME, so hermetic harnesses should set it to an empty directory instead.",
  rbd=[("Clearing CLOUDSDK_CONFIG disables the gcloud fallback entirely, so a hermetic harness only needs to unset it.", 'polarity', 'contradicts')],
  facets=["RESOLVE_ENV_VARS", "point it at an empty directory"])
NA(T, "Does lambo support AWS IAM authentication for RDS Postgres?")
NA(T, "How does lambo rotate Cloud SQL IAM tokens?")
NA(T, "Which Gemini embedding model and dimension does lambo default to?")

# A near-verbatim duplicate from the store: the same advisory written twice,
# one copy with a doubled backslash (from #178 i3-02, texts differ by escaping).
from common import D
P(D("J1 round-3 advisory (J1-R3-3, caps.rs not J1): the INVISIBLE_RANGES decision recorded at is_disallowed_format states U+2028/U+2029 meet the table's own 'invisible structural character' criterion, then declines on the ground 'content already permits \\\\n,")['text'],
  D("J1 round-3 advisory (J1-R3-3, caps.rs not J1): the INVISIBLE_RANGES decision recorded at is_disallowed_format states U+2028/U+2029 meet the table's own 'invisible structural character' criterion, then declines on the ground 'content already permits \\n,")['text'],
  'paraphrase', 'near-verbatim', 'evidence/mooshik-i3-live-ledger/duplicates.txt')
