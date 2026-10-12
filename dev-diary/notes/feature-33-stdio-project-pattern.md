# #33 PR 5: stdio project patterns

## Decisions and reasons

The owner revoked the optional/small cutoff on 2026-10-13 and requested the complete feature. HTTP routing remains unchanged.

Only `*` is special, matching zero or more bytes within the whole final filename. Multiple stars are allowed; all other filename characters are literal and no escape syntax is provided. Lexical validation recognizes native Windows backslash separators as well as slash separators, while retaining explicit star/`.` and star/`..` rejection. Parent-component stars are hard configuration errors, including stars followed by `.` or `..`; trailing slashes are accepted. This narrow grammar avoids a dependency and recursive glob behavior.

Expand HOME and canonicalize the literal parent, enumerate its immediate children, match their actual names byte-exactly, then canonicalize matching directory roots. This preserves literal symlink semantics, including a child symlink pointing outside the parent. Compare canonical cwd with each canonical root using whole components. Select greatest canonical root depth, then literal over pattern. Conflicting equal-priority patterns refuse; equal-session ties agree. Counting literal glob characters was rejected because it invents a specificity order unrelated to canonical containment. A deeper literal or pattern shadows shallower conflicts just as existing literals do.

Filename case and Unicode normalization are exact even on insensitive filesystems; literal filesystem lookup keeps its established normalization behavior. Matching uses OsStr encoded bytes, supporting non-UTF8 names without lossy conversion. Iterative star retry uses constant space, at most pattern-length times name-length work. The immediate-child scan uses constant child storage and linear directory I/O per pattern. Large parents add startup work; there is no recursive scan.

Missing/non-directory parents or failed listings skip the whole pattern. Broken symlinks and files cannot cover canonical directory cwd. A listing iteration error discards partial results to avoid file-order routing. No raw cwd/HOME values enter errors. --session, defaults, cwd-unavailable fallback and unset/relative HOME skip-and-warn behavior are preserved. Recalled #32 memory said unset HOME refused, but accepted source/docs say skip; source governs.

## Validation and review

Eleven new regressions cover siblings/descendants, boundaries and defaults, priority/ties/privacy, tilde and explicit flags, symlink roots outside parents, canonical parent spelling, missing/non-directory parents, grammar and multiple stars, non-UTF8 names, case and Unicode form. Existing literal regressions remain unchanged. The initial focused run found APFS refuses creating invalid UTF-8 names (errno 92); the non-UTF8 regression now always verifies encoded-byte matching and exercises directory integration where the filesystem supports those names. Full lane gate and fresh reviews pending.

## Follow-ups

No dependency, schema, library API or wire changes. HTTP headers and routing are excluded by the approved design.
