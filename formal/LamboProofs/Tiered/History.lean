import LamboProofs.Tiered.Runs

/-
# The tiered store before #319, and the fixes #319 did not take

**Nothing in this file describes the current Rust.** It holds the runs of
`Machine.lean`'s state machine under the mirror as it was before #319
(`Pprev`, `switchRepairs := false`): a re-embed migration written into the old
contract's index (`project_reembed_into_old_index`,
`reembed_migration_misindexed`) and an operator relabel that left the new
contract's index empty (`relabel_leaves_new_index_empty`), both in sync; and
the two candidate fixes the issue offered that #319 did not take: projecting
the batch under its final contract misses the relabel and on a first stamp
indexes vectors the durable store has just quarantined
(`projectFinal_misses_relabel`, `projectFinal_ahead_of_durable`), and
reordering `reembed_all` changes what the durable store keeps
(`reorder_changes_durable`, which #322 then fixed on the durable side). The
current behaviour on the same batches is in `Runs.lean`
(`reembed_migration_held_back`, `reembed_migration_repaired`).

Boundary: the Rust before #319 and fixes never written, so `formal/models.toml`
does not check this file for drift and the docs may not cite it (the anchor
check refuses it). Each run is one interleaving.
-/

namespace Lambo.Tiered

section History

/-- The previous mirror (before the #319 fix), with the tests' backoff: the
pinned runs below are statements about it. -/
def Pprev : Params := { Pt with switchRepairs := false }

/-- Before #319, `project` placed a re-embed batch's new vectors under the old
contract's index, because its `SetEmbedding` comes last. -/
theorem project_reembed_into_old_index :
    project reembedBatch (some 1) = ([(1, 1, some (1, 11)), (1, 2, some (1, 21))], some 2) := by
  decide

def migrated : St := run Pprev seeded ([.write reembedBatch] ++ steps .mirStep 4 ++ [.read 2])

/-- Before #319, a re-embed migration through the tiered store wrote its new
vectors into the old contract's index while the mirror reported in sync, so
reads under the new contract found nothing. -/
theorem reembed_migration_misindexed :
    migrated.d.contract = some 2 ∧ lookup 1 migrated.d.vecs = some (1, 11) ∧
      migrated.tier.sync = .inSync ∧ markerEpoch migrated = some 2 ∧
      answerAt migrated [1, 2] = some [none, none] ∧
      docAt migrated 1 1 = some (1, 11) ∧ docAt migrated 1 2 = some (1, 21) ∧
      (run Pprev migrated ([.loadBegin, .loadEnd] ++ steps .repStep 20 ++ [.read 2])).rep = .idle ∧
      answerAt (run Pprev migrated ([.loadBegin, .loadEnd] ++ steps .repStep 20 ++ [.read 2])) [1, 2]
        = some [none, none] := by decide

/-- The same switch with `SetEmbedding` first (the order `project`'s table and
`a_contract_mismatch_is_refused_and_a_switch_queries_only_the_new_index`
used) landed in the new index under the previous version. -/
example :
    answerAt (run Pprev seeded ([.write [.setContract (some 2), .upsert 1 (some (1, 11)),
      .upsert 2 (some (1, 21))]] ++ steps .mirStep 4 ++ [.read 2])) [1, 2]
      = some [some (1, 11), some (1, 21)] := by decide

/-- **An operator relabel left the new contract's index empty** (the previous
version; the same bug as #319 with `SetEmbedding` first, found by modelling
the fix). `--allow-embedding-mismatch` flushes a lone `SetEmbedding`
(`Graph::replace_embedding_with_operator_override`, a same-kind model rename
that keeps the vectors). The projection writes nothing, the marker advances
and the session stays in sync, so the vectors the durable store now holds
under contract 2 are in no contract-2 document. -/
def relabeled : St := run Pprev seeded ([.write [.setContract (some 2)]] ++ steps .mirStep 2 ++ [.read 2])

/-- Before #319, an operator relabel (`SetEmbedding` alone) through the tiered
store left the new contract's index empty while the mirror reported in sync.
-/
theorem relabel_leaves_new_index_empty :
    relabeled.d.contract = some 2 ∧ lookup 1 relabeled.d.vecs = some (0, 10) ∧
      relabeled.tier.sync = .inSync ∧ markerEpoch relabeled = some 2 ∧
      answerAt relabeled [1, 2] = some [none, none] := by decide

/-! ### The candidates the fix did not take -/

/-- Candidate 1: project the whole batch under its final contract (apply the
batch's `SetEmbedding` before its upserts). -/
def projectFinal (ms : List Mut) (before : Option Nat) : List Op × Option Nat :=
  projectGo (ms.filter fun m => match m with | .setContract _ => false | _ => true)
    (project ms before).2 []

/-- It places the issue's batch... -/
example : projectFinal reembedBatch (some 1) =
    ([(2, 1, some (1, 11)), (2, 2, some (1, 21))], some 2) := by decide

/-- ... but a relabel writes nothing while the durable session keeps its
vectors under the new contract, -/
theorem projectFinal_misses_relabel :
    projectFinal [.setContract (some 2)] (some 1) = ([], some 2) ∧
      (applyBatch seeded.d [.setContract (some 2)]).vecs = [(2, 0, 20), (1, 0, 10)] ∧
      (applyBatch seeded.d [.setContract (some 2)]).contract = some 2 := by decide

/-- A session with vectors and no contract (a legacy session; the vectors
here stand for any the batch writes before its first stamp). -/
def legacy : Durable := { vecs := [(2, 0, 20), (1, 0, 10)] }

/-- ... and on a first stamp it indexes vectors the durable store has just
quarantined: the stores apply the stamp's legacy quarantine to the batch's
own earlier upserts, so the index would serve vectors the authority does not
hold. -/
theorem projectFinal_ahead_of_durable :
    projectFinal reembedBatch none = ([(2, 1, some (1, 11)), (2, 2, some (1, 21))], some 2) ∧
      (applyBatch legacy reembedBatch).vecs = [] := by decide

/-- Candidate 3: reorder `reembed_all`'s emit order. The durable stores are
not order-insensitive here: with the `SetEmbedding` last, a first stamp
quarantines the batch's vectors; with it first, they survive. Reordering
changes what the durable store holds, so it is not a mirror fix. -/
theorem reorder_changes_durable :
    (applyBatch legacy reembedBatch).vecs = [] ∧
      (applyBatch legacy [.setContract (some 2), .upsert 1 (some (1, 11)),
        .upsert 2 (some (1, 21))]).vecs = [(2, 1, 21), (1, 1, 11)] := by decide

end History

end Lambo.Tiered
