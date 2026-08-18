# Origin, author's draft (unfinished)

Narayan's narrative for the site's Origin page, captured 2026-08-18. **Not
finished** — the author stopped at the CockroachDB beat and will continue.
Kept out of the site until it is done. This file is a working draft, not copy.

---

Hello. I am Narayan.

I am a technical writer. I have been dabbling with tech for over 25 years now.
For someone like me, the past 2-3 years have been a whirlwind. LLM revolution
has transformed how I work, how I play, and may be even how I live.

The idea for Lambo came to me earlier this year. I was trying my best to get
Claude Code and Cursor to behave at work. In spite of having a much faster
utility like ripgrep, the agents would insist on going for grep and glob. Not an
ideal scenario when the information you want is deep inside a large monorepo.

I managed to harness it to a certain level with the help of skills and the base
json files. The behaviour was just not something I could nail down when it came
to subagents and even regular agents.

Another aspect were the memory files that Claude created when you did something.
You might do something one-off and it annoyingly would think I want that one
thing I did at some point of time.

May be behaviour like this made people think LLMs as human-adjacent. But my
thought went the other way. We as humans learn things through associations and
importance. Importance that is earned rather than mentioned once or twice. I am
oversimplifying. Since there are events that are one-off and extremely affecting
that it burns in your brain for life. Even there, you could argue that the
associations and earned importance are doing their work.

LLMs simply do not have such a system in place. There is nothing earned. You try
to artificially reinforce. You stack things in a vector database. You create a
RAG. RAG works great when you want grounded information. But does it really work
in an agentic workflow when there are disparate agents at work? RAG is again
more like a secondary storage. It is not identical to RAM. I was wondering about
a RAM component for the LLM.

The idea of memory evolved in my head. I happened to read a random article one
day. The concept of veneration and canonization in Catholicism came to my mind.
A group coming together to decide someone is worthy of veneration. After some
time, the venerated are canonized. A crude analogy to memory came to my mind.
Not all candidates are venerated. Not all venerated become canonized. I started
writing a specification for Lambo.

Since I do not run in the circles of AI gurus, and my genuine lack of time given
the demands of my professional and personal life, I did not get to share these
ideas with real humans. The spec went through an intensely gruelling review with
many, many LLMs instead. My interest was primarily around how much holes LLMs
can poke into the spec. Eventually, the review rounds and review notes became
much longer than the spec itself.

The question of RAM and secondary storage was being challenged. RAM alone does
not make it good. Canonized entries needs long term storage. Canonized and
venerated needs to be remembered for more than a session. No matter how long
they are, in the larger time frame they are more transient than they appear. My
intitial thought was to use SQLite with vector extensions or any other vector
database. Concurrency would be the blocker then. My thoughts as usual went
towards Postgres and the vector extensions.

When the CockroachDBxAWS hackathon came up, I read up on CockroachDB.
CockroachDB made the design more elegant. It could store graphs, vectors, and
promotions in a single space with the immense possibilities of a distributed
system. Not to mention the independent MCP, that makes the query possible
independent of Lambo.

---

## Notes for finishing it (do not put these on the page)

**The RAM/secondary-storage paragraph describes what actually got built.** Lambo
holds the graph in RAM and flushes write-behind to the durable store; the store
trait is what makes the tier swappable (memory, SQLite, Cockroach). So that
paragraph is not an analogy, it is the architecture, and saying so gives the
piece its spine.

**The analogy survived into the API.** Statuses are `Candidate`, `Venerable`,
`Canonical`, and one of the seven MCP tools is `lambo_saints`. A reader who has
just read the veneration paragraph and then finds a tool called `saints` in the
reference docs gets the thesis in one jump.

**"Earned rather than mentioned once or twice" is implemented literally.** The
gates are blast radius, distinct origin interactions, GC survival and coverage,
with age gates so nothing is promoted by being written three times in a minute.

**Unused CockroachDB material**, if the last beat wants more:

- Serializable isolation is what makes the embedding-contract check a
  transaction rather than a race (`src/store/cockroach.rs:611`, SQLSTATE 40001
  retried).
- The single-writer lease is enforced by the store with a fencing token
  (`src/store/mod.rs:121`), not by convention.
- Three independent MCP clients (OMP, Claude Code, Cursor) returned the same
  `canonization_events` walk, so the promotion record is checkable by tools
  Lambo does not control.
- Beam size 64 was chosen by measurement: the engine default of 32 loses 6-7% of
  true neighbours; 64 reaches recall@50 of 0.990.

**Honesty boundaries for this page:** geo-distribution and multi-region were
never exercised, and the vector index took 85-96s to create on the demo cluster.
The "immense possibilities of a distributed system" line is a possibility, not a
demonstrated property, and the page should not blur that.
