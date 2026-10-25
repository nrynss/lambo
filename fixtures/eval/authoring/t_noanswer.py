"""Hand-written no-answer queries beyond the per-topic ones: near-domain
questions the corpus does not answer, false-premise questions (they presume
something the corpus never says), and out-of-project questions."""
from common import NA, C
for t in [
  "What is lambo's license?",
  "Which Rust edition and minimum supported Rust version does lambo require?",
  "How does lambo shard a session across several Postgres databases?",
  "What is the recommended pgvector HNSW ef_search for lambo?",
  "How do I export a lambo session to Neo4j?",
  "Which MCP protocol version does lambo serve negotiate by default?",
  "How many concepts can a single lambo session hold before recall degrades?",
  "What is the default top_k for lambo_recall?",
  "Does lambo encrypt the SQLite store at rest?",
  "How are lambo credentials stored on Windows?",
  "What is the retention period for ledger heartbeats?",
  "How does lambo handle daylight-saving changes in the event-time clock?",
  "Which embedding model did the G3 spike use for its diffusion experiments?",
  "How often does the daemon tick by default?",
  "What is the GC age threshold for an unreferenced Observation concept?",
  "Which Cockroach version does the live CI job run against?",
  "How do I migrate a lambo store from SQLite to CockroachDB?",
  "What does lambo_stats report as the p99 recall latency?",
  "How many tools does the lambo MCP server expose?",
  "Which agent harness has the highest recall-first compliance on the dogfood rig in October?",
  "What did the K1 NVIDIA leg measure for candle throughput on an RTX GPU?",
  "How much did the hosted Postgres workstream cost in August?",
  "When was workstream D's event-time clock merged?",
  "Which reviewer found the most P1 findings across the J series?",
  "What is the default hot_list_max?",
  "Does lambo support BM25F field weighting in keyword recall?",
  "Which languages does lambo's tokenizer stem?",
  "What was the dogfood store's concept count on 2026-10-01?",
]:
    NA('near-domain', t)
for t in [
  "Why did lambo drop SQLite support in 0.7.0?",
  "Why does the embedder canary refuse attach by default?",
  "When did lambo switch its default embedder to OpenAI text-embedding-3-large?",
  "Why was the semantic merge threshold raised to 0.95 in 0.6.0?",
  "How does lambo's Redis recall tier mirror flushes?",
  "Why does lambo_recall return at most three hits?",
  "Why did #193 allow merges across different version numbers?",
  "How does the vimanam store share concepts with lambo's dogfood store over gRPC?",
  "Why does serve-web allow writes from credentialed tokens?",
  "Which release removed the related_pairs option?",
  "Why does GC collect corrections before the facts they correct?",
  "How does lambo's built-in LLM summarize sessions overnight?",
]:
    NA('false-premise', t, family='false-premise')
for t in [
  "What is the capital of Australia?",
  "How do I make sourdough starter from scratch?",
  "What were the main causes of the 2008 financial crisis?",
  "How many moons does Neptune have?",
  "What is the best way to learn Carnatic music as an adult?",
  "How do I change a bicycle inner tube?",
]:
    NA('off-domain', t, family='off-domain')
