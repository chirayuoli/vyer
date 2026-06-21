# Vyer — friction log (Spendwise production build)

Running log of vyer hiccups / slowness hit while building the Spendwise app **through vyer**.
Companion to `update.md` (vyer feature work); this file is "what slowed me down."

---

## ✅ RESOLVED (2026-06-20) — root cause was the stale Dart grammar

Every issue below traced to one cause: `vyer-index` pinned **tree-sitter-dart 0.0.4**, which predates
Dart 3. **Fix shipped:**
- Bumped `tree-sitter-dart` 0.0.4 → **0.2.0** (`crates/vyer-index/Cargo.toml`), switched the grammar fn
  to the `LANGUAGE.into()` binding, and rewrote `DART_TAGS` for the new node kinds
  (`class_declaration` / `function_declaration` / `method_declaration` / `enum_declaration`, plus
  mixins, extensions, and getters).
- **SCRY-038** (null-aware elements `?x`), **SCRY-039** (record / destructuring patterns), **SCRY-040**
  (enhanced enums): now parse cleanly — covered by the new test
  `dart3_modern_syntax_parses_without_error`.
- **SCRY-041** (cascade method-boundary): the old grammar mis-computed method spans on `..` cascades;
  0.2.0 parses cascades correctly, so symbol-replace spans are now right.
- The atomic-batch "amplification" note is moot in practice — the rejections it amplified were these
  grammar gaps, now gone. (Atomicity is intentionally kept; no non-atomic mode added.)
- Verified: **105 workspace tests pass**, `clippy` clean, Dart extraction still works
  (`dart_pack_extracts_classes_methods_and_functions`), release binary rebuilt. **Reconnect `/mcp`** to
  load it.

_(Original entries kept below as the record.)_

## SCRY-038 — tree-sitter-dart grammar rejects Dart 3.x null-aware collection elements
`code_apply` rejected a file using the Dart 3.x null-aware element `?trailing` inside a `[...]` list
literal: "edit rejected: result does not parse (tree-sitter found a syntax error)". Because the batch
is atomic, the OTHER valid edits in the same call rolled back too, and had to be resent. Workaround:
restructured the widget to avoid the syntax (made the field non-nullable with a default).
**Fix:** bump `tree-sitter-dart` to a grammar that parses null-aware elements (`?expr` / `?...spread`).
Severity: medium — blocks an idiomatic, valid Dart pattern.

## Note — atomic-batch rollback amplifies a single parse rejection
Correct all-or-nothing behavior, but one file tripping a grammar gap (SCRY-038) loses the whole
batch's good edits. The error does name the failing file, so resending is targeted. Idea: an opt-in
non-atomic mode, or per-edit partial-commit, for large mixed batches.

## SCRY-039 — tree-sitter-dart grammar rejects Dart 3 record-destructuring patterns
A subagent's `code_apply` was rejected on a file using a Dart 3 record pattern in a for-in loop
(`for (final (value, label) in kinds)`): "result does not parse (tree-sitter found a syntax error)".
Workaround: replaced the record with a small typed class. Same root cause as SCRY-038 — the bundled
`tree-sitter-dart` grammar predates Dart 3 patterns. **Fix:** bump tree-sitter-dart. The two known
gaps so far: null-aware elements (`?expr`) and record/destructuring patterns.
Severity: medium.

## Note — parallel agents over vyer worked well
Three subagents authored 9 screen/widget files concurrently, each via vyer `@new` against a frozen
spine contract. ~4 min wall-clock; only 4 trivial integration issues (1 missing import, 1 redundant
import, 2 lint infos) — no file conflicts (each agent owned distinct paths). Concurrent `code_apply`
from multiple agents on one vyer server caused no corruption.

## SCRY-040 — tree-sitter-dart grammar rejects Dart 3 enhanced enums
A subagent's `code_apply` was rejected on `enum ReportRange { thisMonth, ... ; String get label {...} }`
(an enum with members AND a getter/method body — Dart 2.17+ enhanced enums). Workaround: plain enum +
a separate `extension` for the getter. This is the THIRD grammar gap (with null-aware elements SCRY-038
and record patterns SCRY-039) — same root cause: the bundled tree-sitter-dart predates modern Dart.
**Fix:** bump tree-sitter-dart. Severity: medium — enhanced enums are extremely common in real code.

## SCRY-041 — symbol-replace mis-detects node boundaries on methods containing cascades
A subagent's symbol-replace (`PATH#method`) on a Dart method that uses cascade syntax (`..`) was
rejected as "result does not parse (unbalanced `}`)" — vyer's apply mis-located the method's node span.
Worked around with native Edit. Likely the stale tree-sitter-dart grammar (same family as SCRY-038/039/
040) or brace-span logic tripping on cascades. Severity: low-medium (anchored edits + native fallback
both work; only the symbol-scoped replace failed).

## Positive (kept for balance)
- Compact responses (SCRY-037) made authoring ~25 files painless — headers + previews, no flooding.
- Per-file parse validation on `@new` caught issues immediately; no false rejections except SCRY-038.
- Atomic multi-file batches + delete+recreate were smooth for the rebuild-in-place.
