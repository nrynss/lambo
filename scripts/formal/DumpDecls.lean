/-
Dump every declaration the Lean models make, one JSON object per line, for
scripts/formal/check-decl-equivalence.py and scripts/formal/audit-theorems.py.

  cd formal && lake env lean --run ../scripts/formal/DumpDecls.lean OUT.jsonl MOD...

MOD is each module to import (`LamboProofs.Drain.Base`, ...). Every constant
whose module is under `LamboProofs` is written, auxiliary ones included, as

  {"name", "private", "module", "kind", "user", "line", "span", "levels",
   "type", "value", "uses", "axioms"}

`user` is true when the declaration has a source range (the elaborator gives
one to every declaration written in the source, and none to the constants it
generates: `foo.proof_1`, `foo.match_1`, `foo.eq_1`, `_auxLemma`, recursors,
...). `type` is the kernel type as `Expr.dbgToString` prints it, so binder
names count; `value` is the same for a definition's body (a theorem's proof is
left out: a proof may change, its statement may not), with binder names'
macro scopes erased (`clean`); a generated constant's binders are named by
depth instead (`canon`). `uses` is every constant
the type and value mention. `axioms` (theorems only) is what `#print axioms`
would answer. Names are printed as Lean prints them; a private name is printed
as its user-facing name, with "private": true.
-/
import Lean

open Lean

def kindOf : ConstantInfo → String
  | .axiomInfo _ => "axiom"
  | .defnInfo _ => "def"
  | .thmInfo _ => "theorem"
  | .opaqueInfo _ => "opaque"
  | .quotInfo _ => "quot"
  | .inductInfo _ => "inductive"
  | .ctorInfo _ => "ctor"
  | .recInfo _ => "rec"

def userName (n : Name) : Name := (privateToUserName? n).getD n

/-- `e` with every binder name's macro scopes erased: a hygienic binder name
(`x._@.LamboProofs.Drain.2566094802._hygCtx._hyg.25`, from `(·.key)`) names
the module it was elaborated in, which a file split changes. Binder names do
not change what a term means. -/
partial def clean : Expr → Expr
  | .lam n t b bi => .lam n.eraseMacroScopes (clean t) (clean b) bi
  | .forallE n t b bi => .forallE n.eraseMacroScopes (clean t) (clean b) bi
  | .letE n t v b nd => .letE n.eraseMacroScopes (clean t) (clean v) (clean b) nd
  | .app f a => .app (clean f) (clean a)
  | .mdata m e => .mdata m (clean e)
  | .proj s i e => .proj s i (clean e)
  | e => e

/-- `e` with each binder named by its depth (`b0`, `b1`, ...), so that two
terms print the same exactly when they are alpha-equivalent. Used for the
constants Lean generates: it reuses a matcher it has already made for an
identical `match`, whatever its binders were called. -/
partial def canon (d : Nat) : Expr → Expr
  | .lam _ t b bi => .lam (.mkSimple s!"b{d}") (canon d t) (canon (d + 1) b) bi
  | .forallE _ t b bi => .forallE (.mkSimple s!"b{d}") (canon d t) (canon (d + 1) b) bi
  | .letE _ t v b nd => .letE (.mkSimple s!"b{d}") (canon d t) (canon d v) (canon (d + 1) b) nd
  | .app f a => .app (canon d f) (canon d a)
  | .mdata m e => .mdata m (canon d e)
  | .proj s i e => .proj s i (canon d e)
  | e => e

def main (args : List String) : IO UInt32 := do
  match args with
  | out :: mods@(_ :: _) =>
    initSearchPath (← findSysroot)
    let env ← importModules (mods.toArray.map fun m => { module := m.toName }) {}
      (trustLevel := 1024)
    let h ← IO.FS.Handle.mk out .write
    let names := env.header.moduleNames
    let mut n := 0
    for (c, ci) in env.constants.map₁.toList do
      let some idx := env.getModuleIdxFor? c | continue
      let mod := names[idx.toNat]!
      unless (`LamboProofs).isPrefixOf mod do continue
      let range := declRangeExt.find? (level := .server) env c
        <|> declRangeExt.find? (level := .exported) env c
      let line := match range with
        | some r => toJson r.selectionRange.pos.line
        | none => Json.null
      let span := match range with
        | some r => toJson [r.range.pos.line, r.range.endPos.line]
        | none => Json.null
      let norm := if range.isSome then clean else canon 0
      let value := match ci with
        | .defnInfo v => toJson (toString (norm v.value))
        | _ => Json.null
      let uses := ci.getUsedConstantsAsSet.toList.map (fun u => toString (userName u))
      let axioms ← match ci with
        | .thmInfo _ => do
          let ctx : Core.Context := { fileName := "<dump>", fileMap := default }
          let (axs, _) ← (collectAxioms c : CoreM _).toIO ctx { env }
          pure (toJson (axs.toList.map toString))
        | _ => pure Json.null
      let j := Json.mkObj [
        ("name", toJson (toString (userName c))),
        ("private", toJson (isPrivateName c)),
        ("module", toJson (toString mod)),
        ("kind", toJson (kindOf ci)),
        ("user", toJson range.isSome),
        ("line", line),
        ("span", span),
        ("levels", toJson (ci.levelParams.map toString)),
        ("type", toJson (toString (norm ci.type))),
        ("value", value),
        ("uses", toJson uses),
        ("axioms", axioms)]
      h.putStrLn j.compress
      n := n + 1
    IO.eprintln s!"{n} constants from {mods.length} modules"
    return 0
  | _ =>
    IO.eprintln "usage: lake env lean --run DumpDecls.lean OUT.jsonl MODULE..."
    return 2
