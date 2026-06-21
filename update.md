# Vyer — improvement register

> **Purpose.** This is the living, canonical collection of everything that needs to change for vyer to
> improve a lot — bugs, capability gaps, and feature work — each with evidence and a proposed fix.
> Append to it; don't let findings scatter across chats. Entries are tracked by ID (`SCRY-NNN`).
>
> **Sources of evidence.**
> - **Task** — a real session: *"use vyer to add dense line-by-line comments to every file in
>   `playground/snake`"* (read→edit loop, ~50 edits across 6 files).
> - **Bench** — Track A mechanism micro-bench (vyer warm MCP vs native engines). Numbers and method in
>   `results/report.md`, `benchmark-plan.md`; harness in `benches/track_a/`.
>
> Scope: the *consumer* (agent) side — the MCP `code` / `code_apply` surface. Where an entry contradicts
> a guarantee in `CLAUDE.md`, that is called out explicitly.

---

## 0. Issue index

| ID | Title | Severity | Area | Status |
|---|---|---|---|---|
| SCRY-001 | `parse=ok` is unsound — silent bad write of invalid Python | **CRITICAL** | apply / safety | **FIXED ✓** |
| SCRY-002 | Module-level code unreachable by `code_apply` | HIGH | apply / coverage | **FIXED ✓** (anchored file-scope + read-by-path) |
| SCRY-003 | No whole-file / read-by-path | HIGH | read | **FIXED ✓** |
| SCRY-004 | No sub-symbol / anchored edits (full-body resend only) | HIGH | apply / risk+cost | **FIXED ✓** |
| SCRY-005 | Class-qualified locators unsupported | MEDIUM | apply / addressing | **FIXED ✓** |
| SCRY-006 | Weak `code_apply` miss diagnostics | MEDIUM | apply / DX | **FIXED ✓** |
| SCRY-007 | No file-tree / discovery affordance | MEDIUM | read / DX | open |
| SCRY-008 | Read/edit round-trip latency vs in-process native | LOW | transport | accepted/inherent |
| SCRY-009 | First-class `annotate` (comment) primitive | FEATURE | apply | **✓ via SCRY-004** (anchored edit covers it) |
| SCRY-010 | Post-apply verification beyond parse (tests/typecheck) | FEATURE | apply / safety | proposed |
| SCRY-011 | Atomic multi-file / multi-edit transactions | FEATURE | apply / correctness | proposed |
| SCRY-012 | Token-cost transparency in responses | FEATURE | output | proposed |
| SCRY-013 | `code_apply` cannot insert a new symbol (replace-only) | HIGH | apply / coverage | **FIXED ✓** |
| SCRY-014 | `code_apply` cannot create a new file | HIGH | apply / coverage | **FIXED ✓** |
| SCRY-015 | Locator `blake3` is per-file, not per-symbol | HIGH | locator / staleness | **FIXED ✓** |
| SCRY-016 | `mode=structural` AST-pattern search unimplemented, fails silently | MEDIUM | search | **FIXED ✓** (`mode=ast`, SP-7) |
| SCRY-017 | `detail=outline` doesn't expand class members | MEDIUM | read | **FIXED ✓** |
| SCRY-018 | `parse=ok` passes undefined-name / unimported code | MEDIUM | apply / safety | open |
| SCRY-019 | vyer↔native edit interleaving forces redundant re-Reads | LOW | workflow | open |
| SCRY-020 | Multi-language extraction incomplete (TS class/interface, JS arrow, Go method) | HIGH | parsing / coverage | **FIXED ✓** (Go+JS+TS) |
| SCRY-021 | ~~No regex in lexical search~~ → results symbol-granular, matched line hidden | LOW | search | **CORRECTED** |
| SCRY-022 | `mode=semantic` advertised but disabled; returns generic no-match | MEDIUM | search / honesty | **FIXED ✓** |
| SCRY-026 | No delete operation (symbol or file) | MEDIUM | apply / coverage | **FIXED ✓** |
| SCRY-027 | **Superpower:** repo-wide symbol-aware rename | FEATURE | apply / refactor | **FIXED ✓** |
| SCRY-028 | Impact analysis O(N²) — 4.7 s on a 2k-file repo | HIGH | perf / scale | **FIXED ✓** (inverted index, 570×) |
| SCRY-023 | `exclude_seen` filters post-truncation → can't page, drops real matches | MEDIUM | search | **FIXED ✓** |
| SCRY-024 | Batch queries return one fused list with no per-query attribution | MEDIUM | output | RESOLVED (per-query `found:` note) |
| SCRY-025 | repo-map: weak PageRank spread, indexes non-code, unqualified dup symbols | LOW | repo-map | open |
| SCRY-038 | Inverted index keyed by whole identifiers → substring/prefix lexical queries pruned to zero (recall bug) | HIGH | search / recall | **FIXED ✓** (substring-union over postings keys, plain + boolean paths) |
| SCRY-039 | `mode=auto` only *flagged* low confidence; never escalated to semantic (Rule §5 unfulfilled) | MEDIUM | search / recall | **FIXED ✓** (auto re-fuses semantic@0.2 on close/empty; recovers NL queries) |
| SCRY-040 | No way to EXCLUDE paths from a search (tests/vendor noise) | MEDIUM | search / DX | **FIXED ✓** (`!`-prefixed exclusion globs in `path_scope`, all modes) |
| SCRY-041 | `PATTERN_NO_MATCH` hint was one generic string | LOW | DX | **FIXED ✓** (hint tailored to query shape: multi-word/symbol, restrictive scope, lang) |
| SCRY-042 | `lang` filter accepted only one language | LOW | search / DX | **FIXED ✓** (comma-separated `lang=ts,js` for polyglot repos; union of extensions) |
| SCRY-043 | `detail=context`/`impact` polluted by param names + comment/string words (any matching identifier) | MEDIUM | graph / precision | **FIXED ✓** (unified `scan_idents`: call-site `name(` for `[calls]`, comment/string-aware refs for `[called by]` + impact blast-radius; `validate_reparse` 10→1 callees) |
| SCRY-044 | `detail=count` rejected boolean queries (could search but not count all_of/any_of/none_of) | LOW | search / DX | **FIXED ✓** (boolean count: matching lines/file via `search_bool`, AND-postings pruned) |
| SCRY-046 | No safe LOCAL rename: repo-wide `rename` clobbers other scopes, `anchor` forbids duplicates | MEDIUM | apply / refactor | **FIXED ✓** (`word:true` — whole-word replace-all scoped to one symbol's body, re-parse-validated) |
| SCRY-047 | Multi-word literal queries (`fn parse`) weren't index-pruned → scanned every file (warm p95≈22ms) | MEDIUM | search / perf | **FIXED ✓** (`literal_phrase_tokens` AND-prunes pure literals by their tokens; p95 22→8ms, recall preserved, regex still full-scans) |
| SCRY-049 | Empty query `q=""` matched EVERY symbol/line (`name.contains("")` true) → dumped whole repo into context | MEDIUM | search / context-safety | **FIXED ✓** (dogfound; empty q with no path/boolean at the fused stage → PATTERN_NO_MATCH, anti Rule §6) |
| SCRY-050 | `move_to` the SAME file DUPLICATED the symbol (dest append read pre-cut text; writes collided) — silent data corruption + compile break | HIGH | apply / data-loss | **FIXED ✓** (dogfound; same-file move refused: `src_tgt==dest_tgt` → error, file untouched) |
| SCRY-051 | `@delete` unusable via the CLI — `cmd_apply` demanded a body for the body-less directive | LOW | apply / CLI | **FIXED ✓** (dogfound; `is_delete` (`#@delete` in locator) added to the body-less set; MCP path was always fine) |
| SCRY-052 | `detail=context` showed a RECURSIVE function as `0 callees` (the `!= target` filter dropped the self-call along with the declaration) | LOW | graph / precision | **FIXED ✓** (dogfound; `count_call_sites` — recurses iff `name(` appears ≥2× → keep self-callee; `fact` now `[calls] fact`) |
| SCRY-053 | `detail=context` on an ambiguous name silently analyzed only the FIRST def (showed `2 def(s)` but no guidance) | LOW | graph / DX | **FIXED ✓** (dogfound; multi-def note names which def the [calls]/snippet reflect + lists the others to disambiguate) |
| SCRY-054 | `exclude_seen` paging dead-ended beyond `k` per file — the candidate POOL was capped at `k` (SCRY-023 filtered before take-k but the pool was small) → iterative search dropped matches | MEDIUM | search / paging | **FIXED ✓** (dogfound; when paging, build a pool of ≥256 hits/file so successive pages reveal new matches; non-paging keeps tight `k`, bench unaffected) |
| SCRY-025b | repo-map per-file symbol list repeated a name shared by a struct + its impl (`[Locator, Locator, ...]`) — wasted a slot, looked broken | LOW | repo-map / quality | **FIXED ✓** (dogfound by inspecting `vyer map`; order-preserving dedup → redundant name dropped, a distinct symbol surfaces in its place) |
| SCRY-055 | a FAILED `--verify` reported the error but never said the edit had already committed (verify runs post-apply, no rollback) — agent couldn't tell broken code was on disk | LOW | apply / DX | **FIXED ✓** (dogfound; failed-verify message now notes the edit IS written + suggests `undo:1` to revert) |
| SCRY-056 | the locator's `:: blake3=HEX` staleness guard (CLAUDE.md §5) was DISCARDED by `code_apply` — edits from a stale read (symbol changed since) applied silently; contract-vs-behavior gap | MEDIUM | apply / safety | **FIXED ✓** (dogfound by checking §5 vs code; when a locator carries a hash, the symbol's current content must still match it, else refuse with a re-query hint; symbol-anchored so line drift alone never trips it; hashless locators unaffected) |
| SCRY-025c | repo-map (the orientation Resource) ranked machine-GENERATED files (`db.g.dart`) at the very TOP — an agent's first action surfaced codegen, not the sources it should read | LOW | repo-map / relevance | **FIXED ✓** (found via an end-to-end onboarding dogfood; `is_generated` demotes codegen suffixes ×0.2 + tags `(gen)`; `db.dart` now outranks `db.g.dart`; still listed) |
| SCRY-057 | `auto` missed a symbol the agent NAMED in prose ("how does `pack` choose what to keep") — whole-phrase lexical/structural don't match and tf-idf ranked it below a paraphrase | LOW | search / NL-recall | **ADDED ✓** (found via NL-discovery dogfood; `symbol_mention_ids` fuses exact symbol-name words (≥4 chars, stop-word-guarded) into the auto escalation at weight 0.3; `pack` now surfaces; `files`-style generic words guarded so no noise) |
| SCRY-058 | a typo'd/unrecognized `mode` OR `detail` (e.g. `lexcial`/`snippset`) silently fell back (auto/snippet) — the agent's param was ignored with no notice | LOW | search / DX | **FIXED ✓** (found via error-message audit; the envelope now notes `unknown mode/detail X — used ...; valid: ...` while still serving; valid + branch values produce no note) |
| SCRY-059 | `detail=refs` counted a name appearing only in a COMMENT or STRING as a reference (raw line-match) — inflated the count + showed noise; context/impact already excluded these (SCRY-043) | MEDIUM | graph / precision | **FIXED ✓** (found via graph-precision audit; new `code_ident_lines` (comment/string-aware, mirrors scan_idents, kept separate so the critical path is untouched) filters refs to CODE lines; refs 3→1 on the test) |
| SCRY-060 | comment-awareness (SCRY-043/059) only handled C-style `//` and `/* */` — a name in a Python/Ruby `#` comment STILL counted as a reference/caller (refs/context/impact) | MEDIUM | graph / precision (Python/Ruby) | **FIXED ✓** (found probing the block-comment edge; `hash_is_comment_lang` (Python/Ruby) gates a `#` line-comment skip in BOTH `code_ident_lines` and `scan_idents`; `#` left alone in Rust(attr)/JS(private field); Python refs 2→1) |
| SCRY-061 | `detail=refs` scanned EVERY scoped file (search_text + code_ident_lines) — no inverted-index prune (unlike lexical), O(repo) per refs query | LOW | graph / perf-scale | **FIXED ✓** (found auditing the SCRY-059 cost; prune the reference scan to name-containing files via `pruned_lex_files`; sound (a ref only exists where the name does); cross-file refs preserved (test); def loop stays full — cheap memoized symbols) |
| SCRY-062 | `detail=context` ran `scan_idents` on EVERY symbol's body across ALL files (caller detection) — the slowest mode (~17ms warm p95) | MEDIUM | graph / perf | **FIXED ✓** (found via empirical graph-SLO measurement; split into a cheap full pass (names+defs) + an expensive pass pruned to target-containing files — a caller must reference the target; warm p95 17.6→8.0ms (×2); cross-file callers preserved (test); callees/by_name unaffected) |
| SCRY-063 | a large file's `detail=outline` (read-by-path) built ONE span of every signature — over budget it was packed out ENTIRELY, so a 1500-symbol file outlined to an EMPTY result | MEDIUM | output / large-file | **FIXED ✓** (found stress-testing a 6000-line file; cap the signature list to the token budget + append `… +N more symbols` note; small files unaffected; test `large_file_outline_caps_to_budget`) |
| SCRY-064 | a batch with OVERLAPPING queries (both matching the same span) duplicated that span in the output — the agent paid tokens twice | LOW | output / efficiency | **FIXED ✓** (found auditing the batch path; order-preserving dedup by span id after the marks are finalized — per-query attribution stays raw (match counts), only the OUTPUT is deduped; test `batch_dedups_overlapping_spans`) |
| SCRY-065 | genuine REGEX queries (e.g. `validate_\w+`, `error.*handler`) weren't index-pruned at all — full repo scan (the last un-pruned query class) | LOW | search / perf-scale | **ADDED ✓** (`regex_required_prefix` extracts a regex's required literal PREFIX — every match begins with it, so every matching file contains it; AND-prune by it. SOUND: top-level `|` alternation + metachar-start + quantified-optional-last-char all bail to full scan. **An initial version regressed recall on alternation — the existing SCRY-047 test caught it instantly; fixed with a depth-0 `|` guard.** Recall test `regex_prefix_prune_preserves_recall`) |
| SCRY-066 | the regex prune (SCRY-065) only used the PREFIX — a common-prefix regex (`error.*handler`) barely pruned | LOW | search / perf-scale | **ADDED ✓** (`regex_required_literals` extracts ALL required literals of a FLAT regex (no `( \| [ {` — whose contents aren't literal text) and AND-prunes by all of them: `error.*handler`→error∩handler, far more selective. Sound by construction (bail covers one-OF char classes, counted quantifiers, groups, alternation); learning from SCRY-065, the full suite passed first try. Recall test `regex_multi_literal_prune_is_sound`) |
| SCRY-067 | **SANDBOX ESCAPE**: a symlinked DIRECTORY component (`outdir`→outside) redirected a `code_apply` write OUTSIDE the project root — the lexical sandbox (pure vyer-core) validates the path STRING but can't resolve symlinks | **HIGH** | apply / security (§9) | **FIXED ✓** (dogfound probing the write-symlink edge; the apply path now canonicalizes each pending write's longest-existing ancestor and refuses if it resolves outside the canonical root — before any disk write, with atomic rollback; legit in-root writes unaffected; `write_through_symlinked_dir_is_refused` + read-side `symlink_to_outside_root_is_not_followed`) |
| SCRY-068 | **§2 CONSISTENCY**: a DISK flush failure (read-only file, disk full) left the warm core AHEAD of disk — the warm core was updated during validation, but the flush-failure handler (`?`→"commit failed") didn't roll it back, so a query returned the un-persisted edit | MEDIUM | apply / freshness (§2) | **FIXED ✓** (dogfound reasoning about the read-only-file edge; on flush failure, reconcile every touched file's warm-core text with its ACTUAL on-disk content (re-read; remove if gone) before reporting the error — no query sees an un-persisted edit. Test `flush_failure_does_not_leave_warm_core_ahead_of_disk`) |
| SCRY-069 | **ATOMICITY / DATA LOSS**: a `move_to` into a read-only dest flushed the source CUT, then the dest WRITE failed — the symbol was cut but not pasted → LOST (violated §11 "a failed batch changes zero files") | MEDIUM | apply / data-loss (§11) | **FIXED ✓** (dogfound reasoning about partial-flush; `commit_pending` PRE-FLIGHTS every write target's writability BEFORE changing any file and refuses otherwise (no files touched, warm core reconciled). Tried temp+rename first but reverted — it bypassed the read-only bit + needed dir-write; pre-flight respects read-only semantics. Test `move_into_readonly_dest_does_not_lose_the_symbol`. Removed dead `DiskOp::flush`) |
| SCRY-070 | **UNDO HISTORY LOSS**: `undo:N` POPPED each batch then wrote — a write failure (read-only restore target) returned an error AFTER popping, so the undo history was LOST (agent couldn't recover) and the batch was partially restored | MEDIUM | apply / recovery | **FIXED ✓** (dogfound extending the read-only probe to the undo path; undo now PEEKS + pre-flights each batch's writability (shared `is_writable_target` helper, SCRY-069) and refuses with the history INTACT rather than popping-then-failing. Test `undo_into_readonly_file_is_refused_and_preserves_history`) |
| SCRY-071 | **UNBOUNDED MEMORY**: the undo `history: Vec<EditBatch>` was pushed on every `code_apply` with NO cap — a long-lived daemon (the resident-core use case) accumulated every batch's pre-edit file contents forever | LOW | apply / resource | **FIXED ✓** (dogfound applying the resource lens to the daemon; `record_history` bounds the history to MAX_UNDO_BATCHES=256, evicting oldest — memory stays flat across a heavy editing session; undo still reaches far beyond real use. Test `undo_history_is_bounded`) |
| SCRY-072 | **UNBOUNDED MEMORY**: the in-memory `audit: Vec<AuditEntry>` grew on EVERY call with no cap (faster leak than history) | LOW | resource | **FIXED ✓** (resource lens; cap at MAX_AUDIT_ENTRIES=10k, evict oldest — the `--audit` file keeps the full append-only record. Test `audit_log_is_bounded`) |
| SCRY-073 | **UNBOUNDED MEMORY**: the `exclude_seen` paging set `seen: HashSet` accumulated span ids across a session with no reset | LOW | resource | **FIXED ✓** (resource lens; on overflow past MAX_SEEN_SPANS=50k, reset a fresh paging cycle before re-adding the current page. Test `seen_paging_set_is_bounded`. The global-session-seen semantic was reviewed and kept — a defensible design choice, not a bug) |
| SCRY-074 | **RENAME CORRUPTS STRINGS/COMMENTS**: repo-wide `rename` (and the scoped `word:true` rename) used a naive whole-word `replace_word` with NO string/comment awareness — so renaming `foo`→`bar` also changed `"foo should not change"` (string DATA) and `// foo` (comment prose), contradicting the graph's precision and the playbook's promise | MEDIUM | apply / correctness | **FIXED ✓** (dogfound applying the correctness lens to the most-used refactoring op; `replace_word` now runs the same string/comment/char-literal state machine as `scan_idents` (gated by `sq_is_string_lang`/`hash_is_comment_lang`) — replaces only in CODE. Both rename call sites fixed; e2e-verified Rust `//` + Python `#`. Test `replace_word_skips_strings_and_comments`) |
| SCRY-075 | **DELETE/MOVE ORPHAN ATTRS/DOCS**: a symbol's tree-sitter span starts at `fn`/`def`, so `@delete`/`move_to` left its preceding `#[inline]`/`#[test]`/`@decorator`/`/// doc` behind — orphaning an attribute RE-decorates the next item (compile error / wrong behavior); on move the trivia was orphaned in source AND missing from dest | MEDIUM | apply / correctness | **FIXED ✓** (dogfound; correctness lens on AST-node-boundary ops. `leading_trivia_start` extends a symbol's span up over its OWN outer attributes/docs/decorators (Rust `#[`, `///`, decorator-lang `@`); never an inner module-level `#![`/`//!` or ambiguous plain comment, stops at a blank. `prepare_delete` extends the cut; `move_to` carries the trivia to the dest (consistent cut+paste). Also handles `/** … */` block docs (JSDoc/Rust) with a `/**`-only guard so plain `/*` license blocks are NEVER over-consumed. e2e Rust @delete+move + Python decorator + TS JSDoc. Tests `delete_and_move_carry_attributes_and_docs`, `leading_trivia_start_carries_docs_but_not_license_or_inner`) |
| SCRY-076 | **@before SPLITS TRIVIA**: `@before:sym` inserted at the symbol's `fn`/`def` line — i.e. BETWEEN its `#[inline]`/`/// doc` and the symbol — splitting the attribute/doc off onto the inserted item (the `#[inline]` then re-attaches to the new item; the symbol loses its doc) | MEDIUM | apply / correctness | **FIXED ✓** (dogfound; correctness lens on the insert ops. `prepare_insert`'s `@before` point now uses `leading_trivia_start` so the insertion lands ABOVE the symbol's own attrs/docs. @after/@into/@new unaffected (trailing/interior). Test `insert_before_keeps_symbol_attached_to_its_docs`) |
| SCRY-077 | **INSERTS NOT AUTO-INDENTED**: `@into`/`@after`/`@before` spliced `new_body` verbatim, so an un-indented member landed at column 0 (mis-formatted in brace langs; SEMANTICALLY BROKEN in Python where indentation matters) — the caller had to compute the container's whitespace by hand | LOW | apply / polish | **FIXED ✓** (dogfound; native-feel polish. `reindent_block` re-bases the inserted text to the anchor symbol's / container member's indent, preserving INTERNAL relative structure (correct multi-line + Python) and idempotent (already-correct input unchanged). e2e Rust field/method + Python method. Tests `reindent_block_rebases_preserving_relative_structure`) |
| SCRY-078 | **PARSE GATE MISSES INVALID PYTHON (§4)**: the deterministic apply's re-parse validation used `root.has_error()`, which flags ERROR nodes (Rust unbalanced brace) but NOT a dedented Python `def`/`class` body — tree-sitter parses that as an EMPTY block + a stray sibling statement with NO error node, so a structurally-broken Python edit was SILENTLY written (violating Rule §4 "no silent bad write"; Rust was correctly rejected) | MEDIUM | apply / safety (§4) | **FIXED ✓** (dogfound following up the SCRY-077 parse-gate observation; AST dump pinpointed the empty `body:(block)`. `has_parse_error` now walks for ERROR **or** MISSING nodes, and (Python-only, since empty `{}` is valid in brace langs) rejects a `def`/`class` with an empty block body. e2e: invalid Python rejected, valid Python + empty Rust `{}` accepted. Test extends `has_parse_error_catches_invalid_python_that_brace_check_missed`) |
| SCRY-079 | **POST-EDIT REBUILD VIOLATES §7 SLO**: the token index is keyed by the global revision, so ANY edit triggered a FULL O(repo) re-tokenize of every file on the next query — ~68ms at 50k files, past the §7 "<50ms incremental re-index" SLO; the agent's edit→query loop paid it on every query after a write | MEDIUM | search / perf-scale (§7) | **FIXED ✓** (dogfound — wrote `examples/reindex_bench.rs` to MEASURE it (and caught + corrected my own broad-substring confound: true cost ~68ms, not the 946ms a pathological query showed). `TokenIndex` now keeps a per-file `(content-hash, tokens)` side-table and `update_token_index` re-tokenizes only files whose hash changed (new `Db::content_hash`, zero-dep) — 50k post-edit 68→18.7ms, 20k 28→6ms, all within SLO; warm unregressed. Freshness PROVEN: property test `token_index_incremental_matches_full_rebuild` (incremental == full build, stale tokens removed) + the §2 freshness suite stays green) |
| SCRY-080 | **SEMANTIC REBUILDS INLINE EVERY QUERY**: `semantic_spans` (the `mode=semantic` path) built the whole-repo doc/df index INLINE on every call — never touching the revision cache — so each semantic query was O(corpus): ~94ms warm on a 50k-file repo (past the snippet SLO). (My iter-175 "rebuild is cheap" was confounded by THIS bug: warm≈post-edit because both rebuilt inline.) | MEDIUM | search / perf-scale | **FIXED ✓** (dogfound by tracing why an inverted-index change to `semantic_ids` didn't move the needle — the hot path was a different fn. `semantic_spans` now reuses the revision-cached `SemanticIndex` and scores only the candidate union from a new subword→docs inverted index (provably the same result set — a doc scores>0 IFF it shares a query subword), with a deterministic score-desc/index-asc tie-break (§9). Warm 94→5.5ms@50k, 36→2.25ms@20k; 193 tests green + e2e determinism check. Post-edit (first semantic query after an edit) still pays the full rebuild (~125ms@50k) — incremental semantic index noted as a future item, complex due to Vec-indexed postings) |
| SCRY-081 | **CONFUSING ERROR for a misplaced @-directive**: a locator like `PATH#foo#@delete` (the @-op in the wrong position) parsed as a symbol named `foo#@delete` and fell through to a bewildering "edit has neither new_body nor lazy_edit" — the agent intended a delete and got a no-edit error with no guidance | LOW | apply / DX | **FIXED ✓** (dogfound by recalling my OWN syntax slip in an earlier probe; `@` is never valid in an identifier, so a symbol part CONTAINING `@` (but not starting with it) is an unambiguous misplaced directive — return a hint pointing at the correct position (`PATH#@delete:foo`, not `PATH#foo#@delete`) + the valid op list. Correct syntax unaffected. Test `misplaced_at_directive_gives_a_helpful_hint`) |
| SCRY-082 | **AMBIGUITY ERROR DIDN'T SHOW THE OPTIONS**: a symbol shared by, e.g., a Rust `struct Cfg` and its `impl Cfg` errored "ambiguous (2 matches); pass a line range" — but NOT which ranges, forcing the agent to re-query (`detail=outline`) for the line numbers before retrying (a wasted round-trip on a VERY common case) | LOW | apply / DX-efficiency | **FIXED ✓** (dogfound via the “where did I stumble” lens on the struct+impl case; `ApplyError::Ambiguous` now carries the matching spans and the message LISTS them as ready-to-append disambiguators: `… disambiguate by appending one to the locator: @L1-3 | @L4-6`. The agent retries directly with `PATH#@into:Cfg@L1-3` — no round-trip. Test `ambiguous_symbol_error_lists_candidate_ranges`) |
| SCRY-083 | **FAILED READ-BY-PATH GAVE A MISLEADING HINT**: reading a typo'd or wrong-dir path (`src/tokn.rs`, `lib/token.rs`) returned "file not indexed — try a relative path", but the path WAS relative; it was a typo — so the hint misdirected, and the agent had to `detail=tree` to find the real name (round-trip) | LOW | search / DX-efficiency | **FIXED ✓** (dogfound via the “where did I stumble” lens probing read typos; a failed read now suggests the closest indexed file by basename Levenshtein ≤ 2 — catches a one/two-char typo AND a right-name/wrong-dir — `did you mean: src/token.rs`; unrelated names degrade to “check the spelling, or detail=tree” (no bogus suggestion). Distance computed only on the error path. Test `read_typo_path_suggests_the_closest_file`) |
| SCRY-084 | **INCONSISTENT line-range error**: a reversed range whose ends were BOTH past EOF (`80-40` on a 5-line file) clamped to `5-5` and returned the last line, while a reversed IN-range (`4-2`) correctly errored — the `start>end` check ran AFTER clamping, hiding the reversal | LOW | search / consistency | **FIXED ✓** (dogfound probing invalid `lines` specs; check `start>end` on the PARSED values BEFORE clamping, so a reversed range errors consistently. Valid range/head/tail/single-clamp all unaffected. Test extends `line_range_grammar`) |
| SCRY-085 | **`mode=graph` SILENTLY OVERRODE `detail=context`/`impact`**: the dispatch did `if detail=="refs" || mode=="graph" { refs; continue }` — so the natural pairing `mode=graph detail=context` short-circuited to REFS and skipped the context/impact branches below, masking the documented `[calls]`/`[called by]` grouping (the playbook's headline `context` superpower) | MEDIUM | search / correctness-DX | **FIXED ✓** (dogfound via a NEW lens — “does the tool actually do what the docs claim?”: verifying the playbook's `context` grouping showed `mode=graph` gave refs instead. Fix: `mode=graph` defaults to refs ONLY when the detail isn't a more-specific graph detail (`context`/`impact`), so the detail drives the op; `detail=refs` + bare graph mode unchanged. e2e all four combos. Test `graph_mode_with_context_detail_routes_to_context_not_refs`) |
| SCRY-086 | **STRUCTURAL SEARCH IGNORED SMART-CASE**: the docs promise “add an uppercase letter to force exact case”, and LEXICAL honors it — but STRUCTURAL lowercased both query and symbol name unconditionally, so `Engine` matched `engine_lower` too (case-insensitive even for an explicitly-cased query) | LOW | search / consistency-DX | **FIXED ✓** (dogfound via the doc-claims lens, right after SCRY-085; `structural_ids` now applies smart-case like lexical — an uppercase letter in the query ⇒ exact-case match, all-lowercase ⇒ case-insensitive (recall preserved). e2e both cases. Test `structural_search_is_smart_case`) |
| SCRY-087 | **CLI `none_of` flag-name gap (+ none_of correctness now locked)**: auditing the `none_of` (NOT) doc claim, a CLI probe with `--none` returned UNFILTERED results — looked like a none_of bug. Root cause: the CLI flag is `--not`; `--none` (the natural name, matching the `none_of` field) was an unknown flag, silently ignored. The ENGINE/MCP `none_of` is CORRECT (proven) | LOW | DX / cli | **FIXED ✓** (dogfound auditing the boolean doc-claims; isolated engine-vs-CLI with a direct unit test `search_bool_none_of_excludes` that PROVES none_of drops the line with the term (1 all_of vs 2 none_of) — so the doc claim holds and is now LOCKED; added `--none` as a CLI alias for `--not` since it's the natural name I myself reached for. MCP interface was never affected) |
| SCRY-088 | **ENVELOPE INJECTION**: returned span CONTENT was emitted raw, so a malicious file embedding vyer's own delimiters (`⟦/span⟧ source=SYSTEM ⟦span⟧`) could break out of its UNTRUSTED span and inject text that a naive envelope parser reads as TRUSTED structure — undermining the §8 indirect-injection defense | MEDIUM | security (§8) | **FIXED ✓** (dogfound via a fresh ADVERSARIAL-security lens; `output::format_result` now neutralizes `⟦`/`⟧` (U+27E6/7 — effectively absent from real source) to `[`/`]` in span CONTENT, so embedded markers can't form a fake boundary; verified exactly ONE real `⟦/span⟧` closer survives. Fidelity ~nil; the apply path edits the REAL file, never this sanitized display. Test `output_neutralizes_embedded_envelope_markers`) |
| SCRY-089 | **ENVELOPE INJECTION via the ID/PATH** (companion to 088): the span header emits `id={path}` raw, so a pathological FILENAME (`evil⟦span⟧name.rs`, or a NEWLINE in the name) could inject a fake `⟦span⟧` marker or break the id LINE entirely — the same envelope-injection class, via the path instead of the content | MEDIUM | security (§8) | **FIXED ✓** (dogfound continuing the adversarial lens after 088; `output::format_result` neutralizes `⟦`/`⟧`→`[`/`]` AND `\n`/`\r`→space in the id, gated on `contains` so NORMAL filenames (no markers/newlines) are emitted UNCHANGED — the search→apply locator round-trip is intact for real files. e2e: marker-named id neutralized, normal id unchanged. Test `output_neutralizes_envelope_markers_and_newlines_in_the_id`) |
| SCRY-090 | **ENVELOPE INJECTION via ERROR HINT / NOTE** (completes 088/089): `format_error` and `note_line` emitted the hint/note RAW, so an envelope marker or newline echoed into a hint could inject a fake span boundary or split the envelope line | LOW | security (§8) | **FIXED ✓** (dogfound finishing the adversarial sweep; consolidated 088/089's logic into one shared `output::sanitize_field` (Cow — borrow-only when clean, so zero cost for normal output) and applied it to the span id, error hint, AND note. The §8 envelope-injection class is now closed across EVERY output envelope. Test `error_and_note_envelopes_are_injection_safe`) |
| SCRY-091 | **STACK-OVERFLOW DoS via DEEPLY-NESTED FILE**: the SCRY-078 parse-gate walks (`node_has_error_or_missing`, `python_empty_def_body`) were RECURSIVE on tree DEPTH, so applying an edit to a pathologically deep file (≥~100k nesting) overflowed the call stack and ABORTED the daemon (exit 134). Confirmed: 50k OK, 100k/300k crash; a QUERY (tree-sitter parse only) survived, isolating it to MY recursive walks | MEDIUM | robustness / DoS (§8) | **FIXED ✓** (dogfound via the adversarial lens (deep-nesting input); both walks rewritten ITERATIVE with an explicit heap-stack frontier — depth is now free. Verified 100k AND 500k apply cleanly (was a hard crash). Test `deep_nesting_does_not_overflow_the_stack` at 120k depth ABORTS the runner if recursion regresses) |
| SCRY-092 | **SILENT skip of large files** (honesty gap): files over `max_file_bytes` (default 1MB) are skipped at index time — correct (lockfiles/minified bundles shouldn't index) but SILENT: a search for a symbol in a legit large file (e.g. a 1.2MB generated client) returned NO_MATCH with NO indication the file exists-but-was-skipped, violating §8 “surface degradations, don't fail silently” | LOW | DX / honesty (§8) | **FIXED ✓** (dogfound via the adversarial oversized-input probe — a 5MB single-line min.js was absent from detail=tree with no explanation; `vyer://status` now lists skipped-large files (`skipped_large(>NB)=K [paths] — use native tools for these`) via an on-demand metadata-only walk mirroring index's dir-pruning (no state to invalidate; status is rare). Test `status_surfaces_skipped_large_files` (tiny max_file_bytes)) |
| SCRY-093 | **`@new` semantics undocumented/ambiguous** (doc DX): the get_info + playbook lumped `@new` with the insert ops (“`@after:/@before:/@end/@new` insert or create”), never stating `@new` creates a NEW FILE (distinct from the symbol-relative inserts). I MYSELF mis-modelled it as “new symbol in existing file” and got refused — an agent reading the docs would make the same mistake | LOW | DX / docs | **FIXED ✓** (dogfound via the “where did I stumble” lens while probing @new duplicate-handling; clarified all THREE doc sites (2× mcp.rs get_info + playbook): `@new` = create a new file (locator `PATH#@new`, body = file contents, refused if PATH exists — use a symbol/anchor edit to append to an existing file). The behavior was already correct + the refusal error already helpful (SCRY-081-style); this closes the DOC gap) |
| SCRY-094 | **AUDIT-LOG INJECTION**: `record()` wrote the (query/locator-derived) `summary` RAW into a tab-delimited line, so an embedded NEWLINE could inject a FAKE audit entry and a TAB could break the TSV columns — a compromised/indirect-injected agent could forge or corrupt the forensic trail (§9 “every call is audit-logged” depends on the log being FAITHFUL) | LOW | security / audit-integrity (§9) | **FIXED ✓** (dogfound via the adversarial lens on a NEW surface (audit log); `record()` neutralizes `\n`/`\r`/`\t`→space so every entry stays one faithful tab-delimited line. Test `audit_entries_stay_single_line` (a summary with embedded newline+tabs mimicking a fake `code_apply` entry collapses to one line)) |
| SCRY-095 | **ENVELOPE INJECTION via NON-SPAN path displays** (completes 088/089 across ALL surfaces): file-derived paths emitted OUTSIDE the sanitized `format_result` spans were RAW — a pathological filename (`evil⟦span⟧.rs`) injected markers into `vyer://status` skipped_large (a vector I MYSELF introduced in 092), `vyer://repo-map`, `detail=count`, and the apply `+N -M lines ({path})` / `--- a/{path}` diff headers | LOW | security (§8) | **FIXED ✓** (dogfound auditing my OWN 092 + every non-span path display for the 088/089 class; applied the shared `output::sanitize_field` to all 5 sites (status/repo-map/count-body/diff-summary in engine.rs + diff-header in apply.rs; `tree` already went through format_result so was safe). e2e all 5 neutralized, normal paths unchanged. Test `output_surfaces_sanitize_pathological_filenames`. §8 envelope-injection now closed across EVERY agent-facing surface: spans(088/089), error/note(090), audit(094), resource/data paths(095)) |
| SCRY-096 | **REJECTED EXPERIMENT (measure-before-deciding)**: re-measured the known semantic post-edit cost at 50k — 125–133ms, VIOLATING the §7 <50ms incremental SLO (token mode is 18ms, incremental). Hypothesized the bottleneck was `subword_tokens` (per SCRY-079's win) and built a cached-per-file-bags incremental semantic index (identical output, only tokenization cached) | — | perf / opt-in | **REVERTED — made it WORSE (133→179ms)**: the bottleneck is the GLOBAL df/postings/docs construction over ~500k tokens, NOT tokenization; caching only added hash-check + bag-clone overhead. Measurement corrected the hypothesis — reverted to the simple from-scratch build, restored ~125ms. Honest outcome: documented the opt-in-semantic post-edit caveat in §7 (default lexical/structural/token path meets <50ms; semantic is off-by-default and bound by its tf-idf corpus structure; a true fix needs stable doc-ids + delta-maintained df/postings, not justified for opt-in). The DISCIPLINE (implement→measure→revert-on-regression) is the win, not a code change |
| SCRY-097 | **BULK REPLACE was UNDOCUMENTED** (versatility/doc gap): a path-GLOB locator with NO #symbol (e.g. `src/**`) does a BULK anchor-replace across every matching file (re-parse-gated, all-or-nothing, per-file occurrence report) — a powerful multi-file capability, but the docs described anchor+replace ONLY as “a unique sub-symbol or module-level snippet” (single file). Agents reading the docs would never discover the bulk form | LOW | DX / docs | **FIXED ✓** (dogfound via the doc-coverage lens after verifying bulk replace works + is re-parse-safe; documented in BOTH get_info (the bulk-glob form inline in the ops list) and the playbook (a “Replace a string across MANY files” recipe with the all-or-nothing + abort-on-unparseable semantics). Behavior was already correct + safe; this closes the discoverability gap) |
| SCRY-098 | **CI GATE didn't ENFORCE the warm SLO it advertised**: gate.sh's “warm SLO” step ran warm_bench and grepped for the output line, but never ASSERTED the §7 thresholds — a perf regression (e.g. p50 jumping 4ms→100ms) would print a bad number yet still pass GREEN. An unenforced SLO is a silent quality hole, esp. for a tool whose whole pitch is SPEED | LOW | CI / quality | **FIXED ✓** (dogfound via the meta-quality lens “does the gate enforce what it claims?”; gate.sh now parses p50/p95 and `fail`s if p50≥30ms or p95≥120ms (the §7 SLO — loose enough that load-induced tail noise won't flake since the median is robust + p95≈20ms even under load, tight enough to catch a gross regression). Verified the guard FAILS on p50=50/p95=150 and PASSES on real numbers; gate prints `warm SLO OK: p50=..<30 p95=..<120`) |
| SCRY-099 | **`.tsx` (REACT/JSX) SILENTLY MIS-PARSED** — high-value: `.tsx` routed to the plain `typescript` tree-sitter grammar, which can't parse JSX (`<Tag>` collides with a TS type-assertion). Result: components mis-bounded or DROPPED with NO parse error (e.g. a one-line arrow `const Card = () => <div/>` swallowed the following `App` component, which vanished from outline/structural/graph). React/.tsx is one of the most common codebases — a major versatility/correctness gap | HIGH | parsing / versatility | **FIXED ✓** (dogfound via the cross-language lens probing .tsx; added `Lang::Tsx` routing `.tsx`→`tree_sitter_typescript::LANGUAGE_TSX` (the JSX-aware superset grammar; reuses TS_TAGS so the same symbols extract). `.ts` stays on the `typescript` grammar so type-assertions don't break (the two grammars genuinely conflict on `<X>`). Compiler-guided exhaustive-match updates across vyer-incr/index/server; sq/hash lang-rules default-correct for Tsx. Verified: all 3 components extract with correct bounds, App findable, .ts assertion intact. Test `tsx_jsx_components_are_extracted_with_correct_bounds`) |
| SCRY-100 | **MODERN TS/JS EXTENSIONS UNRECOGNIZED** (`.mts`/`.cts`/`.cjs`): `detect_lang` only knew `.ts`/`.js`/`.jsx`/`.mjs`, so ESM/CommonJS TypeScript (`.mts`/`.cts`, standard since TS 4.7) and CommonJS JS (`.cjs`) fell through to `Generic` — INDEXED but ZERO symbols extracted (no outline/structural/graph). Increasingly common in modern Node/TS projects | MEDIUM | parsing / versatility | **FIXED ✓** (dogfound continuing the real-world-file-type lens after .tsx/099; `detect_lang` now maps `.mts`/`.cts`→TypeScript (no JSX, same `typescript` grammar) and `.cjs`→JavaScript (alongside the existing `.mjs`). e2e: all three now extract functions+classes. Test `modern_ts_js_extensions_route_to_the_right_grammar` (also locks .tsx→Tsx) ) |
| SCRY-101 | **8 LANGUAGES SUPPORTED BUT UNDOCUMENTED + lang-filter gap**: vyer has working tree-sitter grammars for Java/Ruby/Swift/Kotlin/C/C++/C#/PHP (verified: all extract symbols), but EVERY doc claimed only 6 tier-1 langs (Rust/Python/JS/TS/Go/Dart). An agent in a Java/C++/C#/etc. codebase would fall back to native tools, never knowing vyer works there — a big versatility-discoverability gap. ALSO: `lang_exts_one` (the `lang:` filter) was missing the `.mts`/`.cts`/`.cjs` I'd just added to detect_lang in 100, so `lang:ts` would miss `.mts` files | MEDIUM | DX / docs / consistency | **FIXED ✓** (dogfound continuing the real-world lens — probed all 8 “hidden” langs, all work; updated the lang list to the full 14 in get_info + README + CLAUDE.md; fixed `lang_exts_one` to include .mts/.cts (ts) + .cjs (js) — the “audit your OWN recent change for the same class” discipline caught the 100 omission. e2e: lang:ts→.mts, lang:js→.cjs, all 8 langs extract) |
| SCRY-102 | **APPLY broke CRLF (Windows) line endings** — editing a CRLF file spliced the new body with LF (`\n`), leaving the edited region LF while untouched lines stayed CRLF → MIXED endings = a noisy whole-line diff on Windows/cross-platform projects (and possible linter/tooling churn). Found probing real-world file variations | MEDIUM | apply / correctness | **FIXED ✓** (dogfound via the real-world-file lens (also confirmed UTF-8 BOM files parse fine + CRLF \r stripped from DISPLAY output); commit_pending now detects a CRLF-dominant on-disk file and normalizes the new text to CRLF via idempotent `to_crlf` (collapse `\r\n`→`\n`→`\r\n`, so untouched CRLF lines are byte-identical, only spliced LF becomes CRLF). LF files untouched (conservative `is_crlf_dominant`: `2*crlf>lf`). e2e: CRLF file stays all-CRLF post-edit, LF file stays LF. Test `apply_preserves_crlf_line_endings`) |
| SCRY-103 | **`.h` C++ HEADERS MIS-PARSED**: `.h` routed to the C grammar, but `.h` is AMBIGUOUS — huge numbers of C++ projects use `.h` (not `.hpp`) for headers. A C++ header's `class`/`namespace`/`template` were SILENTLY DROPPED (only bare method declarations extracted; `class Widget` invisible to outline/structural/graph), no parse error | MEDIUM | parsing / versatility | **FIXED ✓** (dogfound continuing the real-world lens; route `.h`/`.hh`/`.hxx`→C++ grammar (parses C headers too since C is ~a subset, AND extracts C++ class/namespace/template). `.c` stays C. lang_exts_one: `.h` now matches BOTH `lang:c` and `lang:cpp` (it's ambiguous). e2e: C++ header's class extracts (was NO_MATCH), C header's struct still extracts (no regression). Test `c_cpp_header_extensions_route_to_the_right_grammar`) |
| SCRY-104 | **`.pyi`/`.pyw` PYTHON EXTENSIONS UNRECOGNIZED**: `.pyi` (type stubs — typeshed + every typed library ships them) and `.pyw` (Windows GUI scripts) fell to Generic (zero symbols), since `.pyi`/`.pyw` don't end with `.py` | LOW | parsing / versatility | **FIXED ✓** (dogfound continuing the real-world extension lens; detect_lang `.py`/`.pyi`/`.pyw`→Python + lang_exts_one python→[.py,.pyi,.pyw]. e2e: .pyi stub extracts functions+classes (`def f() -> T: ...` is valid Python). Test `python_stub_and_gui_extensions_route_to_python`) |
| SCRY-105 | **NEW: path-scoped rename for MONOREPOS** — `rename` was repo-wide ONLY, so in a monorepo where a name (`handler`/`index`/`config`) recurs across packages as DISTINCT symbols, renaming one would over-rename ALL packages (honest+undoable, but the agent's only precise-scoping option was a TEXT bulk-replace, which isn't symbol-aware). Found via the multi-file/monorepo lens | MEDIUM | apply / versatility (feature) | **ADDED ✓** (dogfound probing monorepo workflows; `Edit.path_scope: Vec<String>` (optional, serde-default empty=repo-wide — backward compatible) confines the symbol-aware rename's file iteration via the existing `path_in_scope` (same globs+`!`exclude as search). e2e: `rename handler→handle --path-scope packages/auth/**` renames auth, leaves billing's distinct `handler` untouched; unscoped still repo-wide. CLI `--path-scope`; documented in get_info + playbook (“Rename within ONE package”). Test `rename_can_be_path_scoped_for_monorepos`) |
| SCRY-106 | **EXTENSIONLESS SCRIPTS (shebang) UNRECOGNIZED**: executables in `bin/`/`scripts/` with NO extension (`deploy`, `manage`, `build`) but a `#!/usr/bin/env python|node|ruby` shebang fell to Generic — only partially extracted (a `def` caught heuristically, a `class` dropped). detect_lang is extension-ONLY; real projects have many extensionless executables | LOW | parsing / versatility | **FIXED ✓** (dogfound via the real-world lens; new `detect_lang_with_text(path,text)` consulted by `set_text` — extension WINS, falls back to a `#!` shebang for python/node/ruby (the langs with grammars; bash/sh stay Generic) only when the extension yields Generic, so a real `.rs`/`.py` is never reinterpreted. e2e: no-ext python script now extracts class+methods, was partial. Test `shebang_detects_language_for_extensionless_scripts`) |
| SCRY-107 | **lang-FILTER missed shebang scripts** (consistency — my OWN 106 gap): SCRY-106 made detect_lang recognize extensionless shebang scripts, but `scoped_files`'s `lang:` filter is EXTENSION-based, so `lang:python` MISSED a no-ext python script (found fine WITHOUT the filter). Same filter-vs-detection class as 100→101 | LOW | DX / consistency | **FIXED ✓** (dogfound by AUDITING my own 106 for the filter-consistency class; `scoped_files` now matches a file if its extension matches OR (it's EXTENSIONLESS and its detected `db.lang` is in the requested set). Extensioned files (incl. `.h`-in-both-c/cpp) keep the fast extension path untouched — the db.lang rescue is gated on `is_extensionless` so it can't over-match. New `lang_enums`/`lang_name_to_enum`/`is_extensionless`. e2e: `lang:python` now finds the shebang script + .py, `lang:rust` doesn't over-match it. Test `lang_filter_helpers_for_shebang_scripts`) |
| SCRY-108 | **MISLEADING omitted-note when NOTHING fit**: if even the top span exceeds budget_tokens (none kept), the `⟦more⟧` note still said “N lower-ranked spans omitted; re-query detail=locate to list” — but the span wasn't lower-ranked, it was the ONLY result and too big; “to list” (1 span) is wrong too. Violates §5 actionable-errors. Found probing huge-function snippets | LOW | DX / honest-errors | **FIXED ✓** (dogfound via the efficiency-edge lens; `format_result` branches on `spans.is_empty()`: nothing-fit → “the top result alone exceeds budget_tokens; raise it, or use detail=locate / read a window with path+lines” (actionable); some-fit → the unchanged “lower-ranked omitted” note. Test `nothing_fit_note_is_actionable_not_mislabeled`) |
| SCRY-109 | **move error less helpful than @into/@after** (consistency): a `move_to` to a non-existent symbol said “no symbol `ghost` in file” but — unlike `@into`/`@after`/`new_body` errors — did NOT list the file's available symbols, so the agent couldn't see the right target without a second round-trip | LOW | DX / honest-errors | **FIXED ✓** (dogfound probing apply-op error consistency; the move's `symbol_text` map_err now appends `— symbols in {path}: ...` (the `src_syms` are already in hand), matching the other ops. e2e verified; existing move success-path tests unchanged) |
| SCRY-110 | **RENAME CORRUPTED BACKTICK STRINGS** (silent data corruption): `replace_word`'s string state machine handled `"`/`'`/comments but NOT backtick — so a rename touched a symbol name inside a JS/TS TEMPLATE LITERAL or a Go RAW STRING (very common langs). e2e: rename `handler`→`process` changed `` `please call handler in this template` `` to `...process...` (silent string corruption; the playbook explicitly claims strings are left untouched) | MEDIUM | apply / correctness | **FIXED ✓** (dogfound via the cross-language rename-precision lens after verifying Rust+Python were fine; added a `backtick_is_string_lang` (JS/TS/Tsx/Go) + a backtick branch to `replace_word` (copy verbatim to closing backtick, no `\`-unescape since Go raw strings have none). e2e: JS template TEXT + Go raw string now preserved, symbol still renamed. Test extends `replace_word_skips_strings_and_comments`. NOTE: a `${}` interpolation is treated as string-text too (a rename inside it is skipped — recoverable/compile-caught, vs the silent corruption it replaces). EXTENDED to the graph scanners (scan_idents/code_ident_lines) too — refs/context/impact no longer count a name mentioned only in a backtick string (e2e: JS `target` refs=1 not 2), so strings AND comments are uniformly excluded from the reference graph. Test extends `scan_idents_edge_cases`) |
| SCRY-111 | **RENAME/GRAPH MIS-HANDLED TRIPLE-QUOTED docstrings** (Python/Dart `"""`/`'''`): the scanners had no explicit triple-quote handling — the sequential quote-pairing accidentally worked for the COMMON cases (no/ balanced internal quotes, multi-line) but DESYNCED on an ODD number of internal quotes (e.g. a `5"` measurement), wrongly treating a trailing symbol mention in the docstring as code → rename corrupted it / graph over-counted it | LOW | apply+graph / correctness | **FIXED ✓** (dogfound continuing the cross-language lens after 110; added `triple_quote_lang` (Python/Dart) + an explicit `"""`/`'''` branch (skip to the closing triple, BEFORE the single-quote handler) to ALL THREE ident scanners (replace_word + scan_idents + code_ident_lines). e2e: a docstring `"""Max 5" then call handler."""` no longer renames `handler`. Test extends `replace_word_skips_strings_and_comments`. NOTE: the scanners now take 4 `*_lang` bools — a future cleanup is to pass `lang` once and derive them internally) |
| SCRY-112 | **RENAME CORRUPTED RUST RAW STRINGS** (silent data corruption): `r"..."`/`r#"..."#` hold internal `"` verbatim (used for regexes/JSON/SQL/paths), but the scanners had no raw-string handling — the normal `"` handler ended early on the first internal quote, so a symbol mentioned inside got renamed. e2e: rename `handler`→`process` changed `r#"run "handler" inside"#` to `...process...` (3 occ not 2). Rust is tier-1; the playbook PROMISES “a rename never corrupts string data” | MEDIUM | apply / correctness | **FIXED ✓** (3rd string-syntax bug from the cross-language lens after backtick(110)+triple(111); added `raw_string_lang` (Rust) + a raw-string branch to `replace_word`: detect `r`+`#`*+`"` (only when `r` isn't mid-identifier), skip to the hash-balanced close `"`+same-#-count. e2e: raw-string text preserved, symbol renamed. Test extends `replace_word_skips_strings_and_comments`. SCOPE: fixed in the RENAME path (the corruption); the graph scanners (scan_idents/code_ident_lines) still count raw-string mentions — LOW within `graph=approx`, deferred to the consolidation refactor. **The scanners now thread 5/4 `*_lang` bools — the lang-consolidation refactor (pass `lang` once, handle every string syntax incl. raw in ONE place) is now NECESSARY, not optional**) |
| SCRY-113 | **lang-CONSOLIDATION refactor** (cleanup + completes 110/111/112): after 3 string-syntax fixes the 3 ident scanners threaded 4–5 `*_lang` bool params through every call site — error-prone + each new syntax meant touching ~18 sites | n/a (refactor) | code-quality + graph correctness | **DONE ✓** (changed `replace_word`/`scan_idents`/`code_ident_lines` to take `lang: vyer_incr::Lang` ONCE and derive ALL string-syntax flags (sq/hash/backtick/triple/raw) internally — a new syntax is now added in ONE place. Collapsed 6 engine call sites + migrated 13 test calls to a representative `Lang`. BONUS: extending it gave the GRAPH scanners raw-string handling for free — closes the SCRY-112 deferred gap (e2e: Rust `target` refs=1 not 2, raw-string mention no longer a false reference). All 3 scanners now uniformly exclude line/block comments + sq/double/backtick/triple/raw strings. 216 tests green; gate clean) |
| SCRY-114 | **NEW: `vyer init` — bootstrap the agent's CLAUDE.md to prefer Vyer** (adoption/discoverability): an MCP server can PERSUADE via get_info instructions but can't FORCE an agent off native tools; the reliable lever is a CLAUDE.md rule, which users had to write by hand | n/a (new feature) | adoption / DX | **DONE ✓** (new CLI subcommand `vyer init [--root P | --global | --path FILE] [--dry-run]`: writes a MANAGED block (sentinel-delimited `<!-- BEGIN/END SCRY MANAGED BLOCK -->`) into CLAUDE.md teaching the agent to prefer code/code_apply over Read/Grep/Edit + WORK IN BATCHES. Idempotent (re-run = no-op / in-place update of an outdated block), surgical (never touches the user's own content), `--dry-run` previews, `--global`=~/.claude/CLAUDE.md. Pure `upsert_managed_block` is unit-tested (create/append/idempotent/replace). ALSO strengthened the get_info connect-time instructions to lead with “WORK IN BATCHES, not one-at-a-time”. 217 tests; gate green) |
| SCRY-045 | `scan_idents` treated `'` as a string delimiter everywhere → Rust lifetimes (`'a`) swallowed surrounding calls in context/impact (recall regression) | MEDIUM | graph / correctness | **FIXED ✓** (lang-aware `'`: char/lifetime in Rust-like langs, string in Python/JS via `sq_is_string_lang(detect_lang)`) |

> **Thematic finding (§8): vyer can *modify* existing structure but cannot *author* new structure.**
> Creating a file, adding an import, or inserting a new function/method/test all force native tools.
>
> **Thematic finding (§9): native tools are language-agnostic text; vyer's structural layer can only
> see what its parsers extract — so parser gaps (SCRY-020) become blind spots native tools never have,
> and its search lacks regex (SCRY-021), the agent's most-used primitive.** Where vyer is *structured*
> it is excellent; where the structure is missing or the query is a raw pattern, it underperforms a
> plain `grep` — the opposite of "surpasses native everywhere."

**Severity legend.** CRITICAL = can silently corrupt code or breaks a stated guarantee · HIGH = blocks
replacing a native tool for common work · MEDIUM = forces fallback/trial-and-error · LOW = inherent
trade-off, document don't necessarily fix · FEATURE = net-new capability to surpass native.

---

## 1. Bugs & correctness

### SCRY-001 — `parse=ok` is unsound; vyer silently writes invalid Python  ⚠️ CRITICAL
**Guarantee broken:** CLAUDE.md Rule 4 — *"re-parse to validate (reject if it doesn't parse — no
silent bad write)."* and §9 security ("no silent bad write").

**Symptom.** `code_apply` accepted a body CPython cannot parse, reported `parse=ok`, and **wrote it**.

**Evidence (Bench E5, `results/report.md`).**
```
new_body: return self.over (((  # broken      # unterminated parens
vyer response:  (written; parse=ok; warm core updated)
CPython oracle: SyntaxError: '(' was never closed  (line 116)
py_compile:     returncode 1 (does not compile)
```
Reproduce: `python3 benches/track_a/edit_bench.py` → row `E5 FALSE parse=ok (silent bad write) = True`.

**Root cause (hypothesis).** Validation uses **tree-sitter**, whose error-recovery produces a tree with
`ERROR`/`MISSING` nodes instead of failing. vyer treats "got a tree" as "parses," so syntactically
invalid code passes.

**Why it matters.** A *false* pass is worse than native's honest no-validation: the agent trusts the
green signal and moves on, leaving broken code. This undermines the core "deterministic, safe apply"
selling point.

**Proposed fix.**
1. After the splice, walk the new node's subtree; **reject if any `ERROR` or `MISSING` node exists**.
2. Optionally, a per-language native syntax gate (CPython `compile()`, `tsc --noEmit`, `rustc
   --parse-only`/`syn`) behind a fast path.
3. Make the response state the validator used and its limits (`parse=ok(tree-sitter; not type-checked)`).
**Effort:** small for (1); it closes the demonstrated hole. **Regression test:** add the E5 case to the
freshness/security test class so it stays closed.

---

## 2. Capability gaps (block replacing a native tool)

### SCRY-002 — Module-level code is unreachable by `code_apply`  (HIGH)
**Symptom.** `code_apply` only addresses **symbol nodes** (function/method/class) via `PATH#SYMBOL`.
Unreachable: `import` blocks, top-level constants (`DIRECTIONS`, `KINDS`, `_KEYMAP`, the colour
palette, `_BASE_DELAY`…), module docstrings, and the `if __name__ == "__main__":` block.

**Evidence (Task).** "Comment *all* the code" was impossible through vyer alone — `highscore.py`,
`snake.py`, `board.py`, `food.py` had to be done with native `Write` because their targets were largely
module-level; module-level lines in the vyer-edited files remain uncommented.

**Impact.** ~30–40% of a typical file can't be touched → vyer can't fully own the edit loop.

**Proposed fix.** Address top-level statements as nodes: `PATH#@module` (whole module body),
`PATH#@imports`, `PATH#@main`, or `PATH#@Lstart-Lend` for an arbitrary statement range. Even a coarse
"module-preamble" node closes most of the gap.

### SCRY-003 — No whole-file / read-by-path  (HIGH)
**Symptom.** `code` always requires a query; there is no `cat path`. A natural-language
`detail=full` query returns `PATTERN_NO_MATCH` (`q="snake body direction movement"`); you must already
know a symbol name.

**Evidence (Task + Bench).** To edit a file you need its bytes, so you fall back to native `Read`.
Bench R1 had to target a symbol (`Game`) to approximate a file read.

**Impact.** Undercuts the "one tool for the read→edit loop" story; forces native `Read`.

**Proposed fix.** `code` with `path=<file>` and no query → return the whole file, line-numbered,
budget-chunked/paginated. This is table stakes to replace `Read`.

**✅ Status (this pass): FIXED.** `Query` gained an optional `path` field (and `q` is now optional).
`code` with `{"path":"game.py"}` returns the whole file line-numbered (`detail=full`, budget-capped
with an omitted-lines note), its symbol signatures (`detail=outline`), or an `N lines, M symbols`
summary (`detail=locate`). Path resolves by exact repo-relative match or unambiguous suffix
(`token.rs` → `src/auth/token.rs`). Bonus: this surfaces **module-level lines** (imports, top-level
constants) that symbol-anchored reads never could — a partial dent in SCRY-002's read side.
(`vyer-server/src/engine.rs`: `read_path_spans`, `resolve_indexed_path`.)

### SCRY-004 — No sub-symbol / anchored edits; full-body resend is the only primitive  (HIGH)
**Symptom.** To change one line you must resend the symbol's **entire** body. No anchored
(`old→new`) edit, no line-range splice, no insert-at-line.

**Evidence (Task).** Resending `main.py#_title_screen` wholesale **dropped a space in the ASCII-art
banner** — a corruption that an anchored edit makes structurally impossible. Caught and re-applied.

**Impact.** More tokens per edit and a real corruption surface for additive changes. Bench shows the
token waste indirectly: edits carry the whole node even when adding one comment.

**Proposed fix.** See **SCRY-009 family**: an anchored patch op
```jsonc
{ "locator": "render.py#_hud", "anchor": "speed = int(round(1.0/game.step_delay))",
  "replace": "speed = int(round(1.0/game.step_delay))  # ticks/sec" }
```
validated the same way (and gated by SCRY-001's stronger check). Single highest-leverage change for the
edit path.

### SCRY-005 — Class-qualified locators unsupported  (MEDIUM)
**Symptom.** `game.py#Game.toggle_autopilot` → `apply failed: no symbol Game.toggle_autopilot`; only
`game.py#toggle_autopilot` works. Two same-named methods in one file are **unaddressable/ambiguous**.

**Evidence (Task).** First apply attempt failed on the class-qualified form; had to retry bare.

**Proposed fix.** Accept `Class.method` / `Class::method`; disambiguate by enclosing scope.

### SCRY-006 — Weak `code_apply` miss diagnostics  (MEDIUM)
**Symptom.** The *read* no-match error is good (actionable hint with next steps). The *apply* miss is
bare: `no symbol X` — no candidates, no grammar.

**Evidence (Task).** SCRY-005's failure gave no hint that dropping the class prefix would work.

**Proposed fix.** On miss, list nearest symbols ("did you mean `game.py#toggle_autopilot`?") and the
accepted locator grammar — mirror the read-side error quality.

### SCRY-007 — No file-tree / discovery affordance  (MEDIUM)
**Symptom.** vyer indexes the repo but exposes no "list files / show tree" to the caller; I used
`Bash ls` to find the 11 files.

**Proposed fix.** `detail=locate` with an empty query, or a `vyer://files` resource returning the
indexed file list + per-file symbol counts.

---

## 3. Performance & transport

### SCRY-008 — Read/edit round-trip latency vs in-process native  (LOW · mostly inherent)
**Bench evidence (`results/report.md`).**
- Raw file read: native in-process **0.03–0.05 ms** vs vyer **2.7 ms** (MCP round-trip + no read-by-path).
- Raw edit: native **0.15 ms** vs vyer **0.9 ms** (vyer also re-parses + diffs + updates the index).

**Reading it.** This is the cost of being a daemon behind an MCP boundary; it is *not* a regression and
is dwarfed by what vyer gives back (validation, freshness, structure). **But** note: vyer's per-op
latency is only justified when the op does more than a read — pair this with SCRY-003 so a plain read
isn't paying edit-path overhead. Keep all ops well under SLO (they are). **Action:** document, don't
chase; revisit only if M/L-scale benches show it growing.

> Counterweight — where the daemon **wins** (don't regress): search latency is **0.3–1.1 ms (vyer)**
> vs **~7 ms (ripgrep cold spawn)** → **6–25× faster**, and the gap should widen at scale. See §5.

---

## 4. Features to *surpass* native (not just match it)

### SCRY-009 — First-class `annotate` (comment) primitive  (FEATURE)
Commenting is common, safe, and high-volume. Expose it directly:
```jsonc
{ "op": "annotate", "locator": "ai.py#_bfs",
  "comments": { "q = deque([start])": "FIFO frontier -> BFS by distance" } }
```
Server inserts trailing `# …` on matching statements, AST-aware (never inside a string/continuation),
re-parses (SCRY-001-grade). **Strictly better than native** for "comment the code": no body resend, no
whitespace risk. (Generalizes from SCRY-004's anchored edit.)

### SCRY-010 — Post-apply verification beyond parse  (FEATURE · safety)
`parse=ok` proves syntax, not behaviour. Optionally run the file's tests / a type check and return it
inline: `written; parse=ok; pytest=12/12; mypy=clean`. Makes vyer the *safest* editor, not just the
structured one. (Natural home for the SCRY-001 native-syntax gate too.)

### SCRY-011 — Atomic multi-file / multi-edit transactions  (FEATURE · correctness)
~50 edits across 6 files in one task; a mid-batch failure would leave the repo half-edited. Offer
all-or-nothing batches with rollback — the warm core's revision model already supports building this.

### SCRY-012 — Token-cost transparency  (FEATURE · output)
Return the token cost of each read/edit so an agent can budget and choose vyer vs native on cost. Ties
into the code-execution-surface endgame in `CLAUDE.md`.

---

## 5. Benchmark scorecard (Track A, XS — keep these from regressing)

From `results/report.md` (snake repo, warm MCP vs native engines, N=200/100):

| Capability | Winner | Margin | Notes |
|---|---|---|---|
| Search latency | **vyer** | 6–25× | warm core vs ripgrep per-call spawn |
| Targeted read tokens (one function) | **vyer** | 2.6× fewer | `snippet` 2.1 KB vs whole-file 5.4 KB |
| Targeted read tokens (locate only) | **vyer** | 25.9× fewer | locator 208 B vs 5.4 KB |
| Whole-file read tokens | tie | ~1.0× | both return the file |
| Raw file-read latency | native | ~50–90× | in-proc vs IPC (SCRY-003/008) |
| Raw edit latency | native | ~6× | vyer does more per op (SCRY-008) |
| Read-after-write freshness | **vyer** | 0 stale / 50 | guarantee confirmed |
| Edit validation soundness | **neither** | — | vyer gives a *false* pass (SCRY-001) ⚠️ |

**Strengths to protect:** search latency, targeted-read token economy, freshness. These are vyer's real
edge over native; every change above should preserve them.

---

## 6. Priority / sequencing

1. **SCRY-001** — fix the unsound `parse=ok` (CRITICAL; small fix; add regression test). Nothing else
   matters if vyer can silently write broken code while claiming success.
2. **SCRY-004 / SCRY-009** — anchored sub-symbol + `annotate` edits (kills the resend-corruption class,
   slashes edit tokens, makes "comment the code" a vyer superpower).
3. **SCRY-003** — whole-file read-by-path (required to truly replace `Read`).
4. **SCRY-013 + SCRY-014 + SCRY-002** — *authoring*: insert new symbols, create files, address
   module-level nodes. Together these are the difference between "edits existing code" and "builds a
   feature." Any real multi-file change needs all three; today each drops to native.
5. **SCRY-015** — per-symbol locator hashes (the staleness guarantee is currently per-file, too coarse).
6. **SCRY-005 + SCRY-006** — class-qualified locators + apply miss diagnostics (kills trial-and-error).
7. **SCRY-021** — real regex/literal search. Regex is the agent's most-used primitive; today vyer
   can't honor it and a plain `grep` beats it for pattern search. The `ripgrep` libs are already linked.
8. **SCRY-020** — complete multi-language extraction (TS class/interface, JS arrow, Go method). Its
   structural edge is worthless in the languages where the structure isn't extracted.
9. **SCRY-016 + SCRY-017** — make `structural`/`outline` deliver what §5 advertises (AST patterns;
   class-member outlines) or fail loudly instead of returning silent-empty.
10. **SCRY-010 + SCRY-018** — post-apply semantic/test gate (makes vyer the safest editor; catches the
    undefined-name / missing-import class `parse=ok` can't).
11. **SCRY-022 + SCRY-023 + SCRY-024** — search honesty/usability: semantic-disabled signal,
    `exclude_seen` paging, per-query batch attribution.
12. **SCRY-011** — atomic batches (large-refactor correctness).
13. **SCRY-007 / SCRY-012 / SCRY-019 / SCRY-025** — discovery, token transparency, handoff, repo-map.

---

## 8. New findings — complex-feature implementation (combo scoring system)

A second, harder task: implement a **combo/multiplier scoring system** — a new `combo.py` module
(`ComboTracker`) wired into `game.py` (reset/tick/_eat), `render.py` (HUD), and `test_game.py` (3 new
tests). End state: **15/15 tests pass**, game + HUD render clean. Driving it through vyer exposed seven
issues the commenting task could not.

### SCRY-013 — `code_apply` cannot insert a new symbol (replace-only)  (HIGH)
**Symptom.** Applying to a not-yet-existing symbol fails instead of inserting.
```
locator game.py#combo_status (new method) -> apply failed: no symbol `combo_status` in file
```
**Impact.** You cannot add a new function/method/class/test to an existing file via vyer. In this task
it forced native edits for the 3 new `test_*` functions. Combined with SCRY-002, vyer can touch
*existing* defs but never grow a file's symbol set.
**Fix.** An insert op: `{"locator":"game.py#@after:is_over","new_body":"…"}` or
`{"insert_into":"test_game.py","after":"test_…","body":"…"}`, re-parsed like any apply.

### SCRY-014 — `code_apply` cannot create a new file  (HIGH)
**Symptom.** `locator combo.py#ComboTracker -> file not indexed: combo.py`.
**Impact.** New modules are impossible through vyer; `combo.py` had to be `Write`-n natively. A
multi-file feature therefore *always* drops out of vyer at the "new file" step.
**Fix.** A `create_file` path (sandboxed like apply) or accept an apply to an unknown path as
"create + add symbol," then index it.

### SCRY-015 — Locator `blake3` is per-file, not per-symbol  (HIGH)
**Guarantee weakened:** CLAUDE.md §5 — *"`:: blake3=HEX` — symbol-anchored … + content hash (detects
staleness)."*
**Evidence.** Every symbol in a file shares one hash:
```
combo.py#ComboTracker  combo.py#register_eat  combo.py#multiplier  combo.py#tick   -> all blake3=49fa3660fa827205
game.py#reset          game.py#_eat           game.py#tick                          -> all blake3=f6288fba972ea73d
```
**Impact.** The hash detects "the *file* changed," not "*this symbol* changed." Two consequences:
(1) editing any one symbol changes the stored hash of **every** other symbol's locator, so a cached
locator for an *untouched* function falsely reads as stale; (2) you can't tell *which* symbol drifted.
The "survives line drift + per-symbol staleness" story is coarser than advertised.
**Fix.** Hash the symbol's own node text (or node text + enclosing path), not the file.

### SCRY-016 — `mode=structural` AST-pattern search is unimplemented and fails silently  (MEDIUM)
**Advertised:** §5 — `q`: *"Text / identifier / AST-ish pattern / natural language (per `mode`)."*
**Evidence.** Under `mode=structural`, only bare symbol *names* match. Both of these returned **zero
spans with no error**:
```
q = "self.count += 1"     (statement / content pattern)  -> (empty)
q = "def $NAME(self)"     (AST-ish pattern)              -> (empty)
```
**Impact.** Structural search is really "symbol-name lookup." Worse, an unsupported pattern returns a
silent empty result that looks identical to "legitimately no matches" — a debugging trap.
**Fix.** Either implement AST-pattern matching (tree-sitter queries) or, at minimum, detect
pattern-shaped queries and return an actionable error (`structural mode matches symbol names; use
mode=lexical for code text, or dump_ast`).

### SCRY-017 — `detail=outline` does not expand container members  (MEDIUM)
**Symptom.** `outline` of a class returns only its header line:
```
detail=outline q=ComboTracker -> "12: class ComboTracker"   (no method signatures)
```
**Impact.** Outline is meant to be "signatures, bodies elided" — the one-call way to see a class's
shape. For containers it gives nothing beyond `locate`, so you must query each method separately
(more round-trips, the opposite of progressive disclosure).
**Fix.** For a class/module node, `outline` should list child signatures (methods/fields) with bodies
elided.

### SCRY-018 — `parse=ok` passes undefined-name / unimported code  (MEDIUM · extends SCRY-001)
**Symptom.** After wiring `ComboTracker` into `game.py` *before* adding its import, vyer reported
`parse=ok` for all three method edits, though the module raises `NameError: ComboTracker` at runtime
until the (module-level, vyer-unreachable) import is added natively.
**Impact.** vyer's validation is purely **syntactic and per-file**; it cannot see that a name is
undefined or that a required import is missing — especially when the fix lives in module-level code
vyer can't even edit (SCRY-002). Green `parse=ok` on a non-importable module is misleading.
**Fix.** Optional post-apply semantic gate (import/name resolution, or just `python -c "import m"`),
surfaced as a separate field: `parse=ok; imports=unresolved(ComboTracker)`. Reinforces SCRY-010.

### SCRY-019 — vyer↔native edit interleaving forces redundant re-Reads  (LOW · workflow)
**Symptom.** A native `Edit` after a vyer `code_apply` on the same file was rejected — *"File has been
modified since read"* — forcing a re-`Read` before the edit could proceed.
**Impact.** Because SCRY-002/013/014 *force* mixing vyer (modify symbols) with native (imports, new
files, inserts), this interleaving is the normal case for any real feature — and vyer's out-of-band
writes don't share freshness state with the harness's native file tools, so each hand-off costs an
extra read/round-trip.
**Fix.** Mostly a harness-integration concern, but vyer could help by emitting the post-edit file
hash/mtime the harness can adopt, or by exposing the full post-edit file text in the apply response so
no re-Read is needed.

### Positives observed (protect / don't regress)
- **Watch indexed a brand-new file immediately** — `combo.py` was queryable on the very next `code`
  call after native creation (no manual reindex). Good.
- **Cross-symbol `refs` worked** (lexical-approx, honestly tiered `graph=partial(approx)
  tier=lexical-approx`): found the `register_eat` call site and both `ComboTracker` references across
  files. Minor cosmetic bug: a doubled label `def [def] def register_eat` in the refs span.

---

## 9. New findings — capability stress round (search, parsing, apply, resources)

A systematic battery against vyer vs ripgrep ground truth, plus sandboxed extraction/apply probes
(`benches/track_a/probe_extract_apply.py`). Several advertised capabilities are weaker than their
schema/docs imply.

### SCRY-020 — Multi-language symbol extraction is incomplete  (HIGH)
**Claim:** CLAUDE.md — tier-1 **Rust / Python / JS / TS / Go**. **Reality (sandbox probe):**

| construct | lang | extracted? |
|---|---|---|
| free fn, struct, `impl` fn | Rust | ✅ |
| every Python construct (nested, async, `@property`, staticmethod, generator, nested class, overloads) | Python | ✅ |
| `export function`, `class` | JS | ✅ |
| **arrow fn** `const x = () => …` | JS | ❌ |
| **`interface`** | TS | ❌ |
| **`class` with typed methods** (`method(): number`) | TS | ❌ |
| **method with receiver** `func (s S) M()` | Go | ❌ |
| top-level `func`, `type struct` | Go | ✅ |

**Impact.** TS classes/interfaces, JS arrow functions, and Go methods are *everywhere* in real code —
and vyer can neither **search** nor **apply** to what it doesn't extract. Native `grep`/`Read` are
text-based and never miss them. For a structural tool this is the most damaging gap: its blind spots
are exactly where it claims an edge.
**Fix.** Audit/upgrade the tree-sitter queries per language (arrow-fn assignments, TS
`interface`/`class_declaration` with type annotations, Go `method_declaration`). Add a per-language
extraction conformance test fixture so coverage can't silently regress.
**✅ Status (this pass): PARTIAL.** *Go receiver methods* (`func (s T) M()`) now extract correctly —
the name derivation was swallowing the receiver group (`vyer-incr::name_from_header`). *JS/TS arrow &
function-expression consts* (`const f = () => …`) are now indexed as named functions spanning the whole
declaration (`vyer-index::collect` + `js_var_fn_name`). **Still open:** TypeScript needs its own
grammar — `.ts/.tsx` currently parse with the *JavaScript* grammar, so typed `class`/`interface` bodies
break. That requires adding a `tree-sitter-typescript` dependency + a `Lang::TypeScript` variant (a
larger change; tracked as the remainder of SCRY-020).

### SCRY-021 — CORRECTED: regex works; the real issue is symbol-granular results  (LOW)
**Original claim (WRONG):** "lexical is fuzzy, not regex." **Reading the source disproved it:**
`src/lexical.rs` uses `grep_regex::RegexMatcher` (ripgrep's engine) with smart-case and a
literal-escape fallback. `mode=lexical` **is** real regex. My earlier `gr[ou]w` "noise"
(`collides_self`, `move`, `reset`) was *correct* — those symbols contain `_grow_pending`, and the
regex `gr[ou]w` matches the `grow` substring inside it. Verified: `co.bo` regex-matches exactly the
`combo` sites. **Lesson:** verify against ground truth before filing — I'd misattributed legitimate
regex hits as fuzz.
**The real (smaller) issue.** Lexical results are **symbol-granular**: a line hit is mapped to its
enclosing symbol, and `detail=locate` shows the *symbol signature*, not the matched line. So you can't
see *why* a symbol matched (which line/substring) without escalating to `detail=snippet`. Native
`grep` shows the matching line directly. **Fix (minor):** in `locate`/`outline`, include the matched
line number + text alongside the symbol id, so a hit is self-explaining.

### SCRY-022 — `mode=semantic` is advertised but disabled, and fails opaquely  (MEDIUM)
**Symptom.** The `code` schema lists `mode: …|semantic`; `vyer://status` honestly says
`semantic=disabled`; but calling `mode=semantic` returns a generic `PATTERN_NO_MATCH` with a hint that
doesn't mention semantic being off. **Impact.** An agent reaching for the "I don't know the symbol
name" discovery mode gets silent nothing and can't tell it's unavailable vs genuinely empty.
**Fix.** Return an explicit envelope: `code=MODE_UNAVAILABLE mode=semantic (disabled; enable Phase-6
or use mode=auto)`. Honesty parity with the status resource.

### SCRY-023 — `exclude_seen` filters after truncation → can't page  (MEDIUM)
**Symptom.** Two sequential `exclude_seen=true` calls for `tick` (k=2): call 1 → `combo.tick`,
`food.tick`; call 2 → `PATTERN_NO_MATCH` — even though `game.py#tick` is a real, *unseen* match.
**Root cause.** It excludes seen spans *after* truncating to top-k, so once the top-k are seen the
result is empty instead of backfilling the next-ranked unseen matches. **Impact.** The advertised
iterative-search loop ("Drop spans already returned earlier this session") can't actually enumerate all
matches — paging dead-ends. **Fix.** Apply `exclude_seen` *before* the top-k cut (over-fetch, filter,
then truncate).

### SCRY-024 — Batch queries fuse into one list with no per-query attribution  (MEDIUM)
**Symptom.** A multi-query `code` call returns a single flat span list. Distinct queries get
concatenated (you must infer which span answers which query); **overlapping** queries get RRF-fused so
you can't separate them at all. **Impact.** The "batch-capable" tool can't be used to answer N
independent questions in one call without ambiguity — undercutting the round-trip-saving pitch.
**Fix.** Group output per query (e.g. `⟦query i=0 q="…"⟧ … ⟦/query⟧`) so attribution is explicit.
**RESOLVED.** A batched (≥2-query) `code` call now appends a `per-query found: q0 `…`→N  q1 …`
note (a marks series over `spans.len()` captured per iteration; deterministic, zero hot-path cost).
The agent sees which queries matched and can re-issue the empty ones — keeping the flat, edge-ordered
span list (Rule §7) intact rather than fragmenting it into per-query blocks. (`engine::code`.)

### SCRY-025 — repo-map quality: weak ranks, non-code files, unqualified dups  (LOW)
**Symptom (`vyer://repo-map`).** PageRank is near-uniform across the 7 core modules
(0.119–0.129 — little signal); the map indexes non-code files (`.snake_highscore.json`,
`.claude/settings.local.json`); and symbol lists show `__init__` twice (FoodKind's and Food's) with no
class qualifier (ties to SCRY-005). Also, leaf-but-important modules (`ai.py`, the autopilot) rank near
the bottom because nothing imports them. **Impact.** As an agent's "where do I look" signal it's noisy
and weakly discriminating. **Fix.** Exclude non-source files; qualify symbols by class; consider
blending in symbol-centrality or edit-recency so import-leaves aren't buried.

### Confirmed strengths this round (protect / don't regress)
- **Python extraction is comprehensive** — nested funcs, `async def`, `@property`, `@staticmethod`,
  generators, nested classes, and *both* overloaded `dup` defs all indexed.
- **Ambiguous-apply is detected, not mis-applied** — editing `dup` (2 defs) is refused:
  *"symbol `dup` is ambiguous (2 matches); pass …"*. Good safety default.
- **`@Lstart-end` locator form works** (`tricky.py#fetch@L13-14`) — and is the disambiguator for
  overloads.
- **Multi-edit in one `code_apply` call works**, with symbol-anchoring surviving the line-shift from an
  earlier edit in the same file.
- **`lazy_edit` is honestly rejected** as a Phase-6 sidecar (no silent mis-apply).
- **MCP Resources work and are honest** — `vyer://status` self-reports `semantic=disabled`,
  `graph=partial(approx)`; `vyer://repo-map` renders. The `lang` filter is honored.

---

## 10. Implemented in this pass (changelog)

Real code changes landed in the vyer workspace; `cargo test` (full suite, **64 tests**) green,
`cargo clippy --all-targets` clean, all verified end-to-end against a freshly-built server.

| ID | Change | Files | Verified |
|---|---|---|---|
| **SCRY-001** | Apply path now gates the spliced result through a **real tree-sitter parse** (`has_parse_error`) and rejects on any `ERROR`/`MISSING` node — closes the silent-bad-write hole (Python had *no* check before). | `vyer-index/src/lib.rs` (`has_parse_error`), `vyer-server/src/engine.rs` (apply gate) | broken Python edit → **rejected** (was: `parse=ok` + written) |
| **SCRY-015** | Locator hash is now computed over the **symbol's own line span**, not the whole file — editing one symbol no longer invalidates every other locator's hash. | `vyer-server/src/engine.rs` (`make_id`, `symbol_slice`) | two symbols in one file → **distinct** hashes |
| **SCRY-020** | **Go receiver methods** name-resolve correctly; **JS/TS arrow & function-expression consts** are indexed. | `vyer-incr/src/lib.rs` (`name_from_header`), `vyer-index/src/lib.rs` (`collect`, `js_var_fn_name`) | `func (s T) Area()` and `const add = …` → **found** |
| **SCRY-023** | `exclude_seen` filters **before** the top-k cut, so iterative paging enumerates all matches instead of dead-ending. | `vyer-server/src/engine.rs` (`code`) | k=1 paging walked **both** `tick` matches then exhausted (was: 2nd call empty) |
| **SCRY-003** | **Read-by-path** added: `code {"path":"f.py"}` returns the whole file (line-numbered, budget-capped), `outline`, or a summary — no query/symbol needed. Resolves exact or unambiguous-suffix paths; surfaces module-level lines symbol reads can't. | `vyer-server/src/engine.rs` (`read_path_spans`, `resolve_indexed_path`; `Query.path`, optional `q`) | whole `game.py` incl. `import`/`CONST` returned ✓ |
| **SCRY-021** | **Corrected** — not a bug. Lexical search is real ripgrep regex; the original finding was a misread. Re-scoped to a minor observability note. | — | `co.bo` → exactly the `combo` sites |

New regression tests: `go_method_receiver_name_is_extracted` (vyer-incr),
`has_parse_error_catches_invalid_python_that_brace_check_missed` + `js_arrow_function_is_extracted`
(vyer-index), and `read_by_path_*` ×3 (vyer-server integration). Full suite now **67 tests**, green.

### Second batch — authoring + anchored edits + TypeScript

| ID | Change | Files | Verified |
|---|---|---|---|
| **SCRY-004** | **Anchored sub-symbol edit**: `code_apply {locator, anchor, replace}` replaces the unique-in-scope `anchor` with `replace` (re-parse validated). No full-body resend, structurally can't corrupt other lines. Also serves SCRY-009 (commenting). | `vyer-server/src/apply.rs` (`prepare_anchored`, `line_diff`), `engine.rs` dispatch, `Edit.{anchor,replace}` | `return SPEED`→`SPEED * 2` applied; ambiguous anchor refused |
| **SCRY-002** | **Module-level edits**: a bare-`PATH` anchored edit scopes to the whole file, so imports/top-level constants are now editable. Combined with read-by-path (SCRY-003), module-level code is fully reachable. | `engine.rs` (file-scope dispatch), `apply.rs` (`symbol=None`) | `SPEED = 5`→`SPEED = 10` at module level applied |
| **SCRY-013** | **Insert a new symbol**: `PATH#@after:SYM` / `#@before:SYM` / `#@end` splice a freshly-authored `new_body` (re-parse validated). | `apply.rs` (`prepare_insert`, `InsertPos`), `engine.rs` dispatch | inserted `reset` method → immediately searchable |
| **SCRY-014** | **Create a new file**: `PATH#@new` with `new_body` writes a new file (parse-validated, refuses overwrite), then indexes it synchronously. | `engine.rs` (`@new` branch, `creation_diff`), `vyer-incr::detect_lang` made public | new `combo.py` created → symbol searchable; overwrite refused |
| **SCRY-020** | **TypeScript grammar wired** (`Lang::TypeScript`, `tree-sitter-typescript`, `.ts/.tsx` routing): TS `class`/`interface`/`enum`/`type`/method/arrow now extract. (Go + JS arrows were batch 1.) | `vyer-incr/src/lib.rs` (`Lang`, `detect_lang`, `parse_text`, modifiers), `vyer-index/src/lib.rs` (`ts_language`, `kind_of`, arrow) | `interface Foo`, `class TsClass`, `tsArrow`, `enum`, `type` all found |

New tests: `anchored_edit_*` ×3 + `insert_after_symbol_and_at_end` (apply unit), `apply_anchored_*` +
`apply_insert_*` + `apply_create_*` ×5 (integration). **Full suite now 76 tests**, `clippy` clean.
All verified end-to-end against a freshly-built server.

### Third batch — delete, navigation polish, and the first *superpower*

| ID | Change | Verified |
|---|---|---|
| **SCRY-026** | **Delete op**: `PATH#@delete:SYM` removes a symbol's node (re-parse validated, swallows a trailing blank); `PATH#@delete` deletes the whole file and drops it from the index (`Db::remove_text`). | symbol & file delete; index has no stale entry |
| **SCRY-005** | **Class-qualified locators**: `Container.method` / `Container::method` resolve the nested same-named member, disambiguating overloaded methods across classes without line numbers. | `a.py#B.m` edits B's method, leaves A's |
| **SCRY-006** | **Apply miss diagnostics**: a "no symbol" error now lists the file's actual symbol names. | error names `validate_token`, … |
| **SCRY-006b** | **Anchored-edit whitespace diagnosis**: when an `anchor` isn't found verbatim but matches whitespace-normalized in scope, the error names the likely cause (indentation / tabs-vs-spaces / trailing ws) instead of a bare "not found". | `AnchorWhitespaceMismatch`; test `anchored_whitespace_mismatch_is_diagnosed` |
| **SCRY-017** | **Class-member outline**: `detail=outline` on a class/struct/impl/trait/interface/enum now lists its members' signatures (bodies elided) — the class's shape in one call. | class outline lists `def m` |
| **SCRY-022** | **Semantic honesty**: `mode=semantic` now emits an explicit "disabled; served hybrid instead" note instead of silently degrading. | note present |
| **SCRY-027 ⭐** | **Repo-wide symbol-aware rename**: `code_apply {locator:"PATH#SYM", rename:"new"}` renames the definition + every whole-word reference across the repo, **re-parses every touched file, and commits all-or-nothing** (validate-all → then write-all). | live: 5 occurrences across 3 files renamed atomically |

New tests: delete ×2, class-qualified, miss-diagnostics, class-member outline, semantic note, rename
×3. **Full suite now 85 tests**, `clippy` clean.

> **Observation (validation depth):** tree-sitter's `has_error()` is *lenient* — renaming an identifier
> to a keyword (`fn`, `def`) does **not** trip it (error-recovery accepts it), and a pure identifier→
> identifier rename essentially can't break the *syntax* tree anyway. So the rename parse-gate guards
> the rare structural break, not semantic/keyword misuse. A stronger gate (SCRY-010/018: a real
> per-language syntax/typecheck pass) would close that — tracked, medium priority.

---

### Fourth batch — the superpower fleet (SP-2/3/6) + benchmarks

| ID | Change | Verified |
|---|---|---|
| **SP-2 ⭐** | **Atomic multi-edit** — every batch is all-or-nothing (deferred disk writes + warm-core rollback). | 3-edit batch w/ 1 bad edit → 0 files changed |
| **SP-3 ⭐** | **Impact/blast-radius** — `detail=impact` returns transitive referrers (depth-tagged). | `base`→`mid`(d1)→`top`(d2) |
| **SP-6 ⭐** | **Undo** — `code_apply {undo:N}` reverts the last N batches (content/create/delete) on disk + core. | rename → undo → back to original; deleted file recreated |
| **Benchmarks** | `benches/track_a/superpower_bench.py` + `results/report.md` §4b: rename = 1 call vs 14–44 native; atomic = 0 vs half-edited. | numbers captured |

Full suite now **91 tests**, `clippy` clean. All four superpowers demoed together live (rename → undo →
atomic-abort).

### Fifth batch — bulk replace, move, keyword gate, absolute-latency proof

| ID | Change | Verified |
|---|---|---|
| **SP-4 ⭐** | **Bulk search-replace** — glob locator + anchor rewrites every match in every file, atomic + parse-validated. | live: 5 occurrences / 2 files |
| **SP-5 ⭐** | **Move symbol across files** — cut from A, append to B (create if needed), re-parse both, atomic. | live: `helper` A→B, searchable in B |
| **SP-8 (partial)** | rename rejects a **reserved-keyword** target (`is_language_keyword`), closing the lenient-tree-sitter hole. | `rename …→fn` rejected |
| **Absolute perf** | every superpower p50 **0.13–0.20 ms** on the warm core (full validated refactors). | benchmarked |

**Six superpowers now shipped** (SP-1…SP-6) + SP-8 partial. Full suite **94 tests**, `clippy` clean.
Only SP-7 (AST-pattern + embedding semantic search) remains — the heaviest, model-dependent item.

### Sixth batch — SP-7 AST-pattern search

| ID | Change | Verified |
|---|---|---|
| **SP-7 ⭐ (pattern half)** | **`mode=ast`** runs a tree-sitter S-expression query over scoped files → matched node spans. Structural search `grep` can't do; invalid queries reported (closes **SCRY-016**). | live: `(call …)` found call sites; `(class_definition …)` found classes |

`vyer-index::ast_query` (+ `streaming-iterator` dep), `engine::ast_spans`, `mode=ast` branch.

### Seventh batch — real semantic search (SP-7 complete)

| ID | Change | Verified |
|---|---|---|
| **SP-7 ⭐ (semantic half)** | **`mode=semantic`** — deterministic subword TF-IDF retrieval; finds a symbol from a natural-language description without the exact name. `status` now reports `semantic=lexical-subword(tf-idf)`. | live: "check whether the auth token is valid" → `validate_token`; "scramble a secret password" → `hash_password` |

`engine::semantic_spans`, `subword_tokens`; status/tests updated. Full suite **95 tests**, `clippy`
clean. **All seven superpowers now shipped (SP-1…SP-7), both halves of SP-7 included** + SP-8 partial.
The *only* remaining items are genuinely optional/heavy: a neural-embedding reranker for true synonymy
(model download) and SP-8's full per-language typecheck/test gate — both with interfaces already in
place.

### Eighth batch — scale validation + a 570× perf fix (SCRY-028)

Built `benches/track_a/scale_bench.py` (generates a 2,001-file / ~20k-line repo) to validate
performance *beyond* tiny fixtures. It immediately caught a latency cliff:

| op @ 2,001 files | before | after |
|---|---|---|
| impact / blast-radius | **4,746 ms** | **8.3 ms** (570× — one-pass identifier inverted index) |
| warm structural search | — | 0.78 ms |
| warm read-by-path | — | 0.36 ms |
| repo-wide rename (6,001 occ / 2,001 files) | — | 53 ms |

All warm ops stay **sub-10 ms at 2,000 files** — the warm core makes scale nearly free; only the
one-time cold index grows. (`engine::impact_spans` rewritten with `identifier_tokens`.) The fix is
exactly the kind of thing that decides whether an agent keeps using a tool: a 4.7 s blast-radius call
would get abandoned; an 8 ms one gets trusted. Full suite **95 tests**, `clippy` clean.

---

### Fourteenth batch — SCRY-031 verify hook + "prefer vyer" MCP instructions

- **SCRY-031 — post-apply verify hook. ✅ DONE (= SP-8 full).** `vyer serve --verify "<cmd>"` (e.g.
  `cargo check`, `pytest -q`, `tsc --noEmit`) runs the command in the repo root after every successful
  write batch and reports the result **inline** in the `code_apply` response: `verify(cargo check)=ok`
  or `verify(...)=FAILED: <first error line>`. This closes the "`parse=ok` ≠ compiles" gap — vyer now
  tells an agent whether an edit *compiles/passes tests*, not just *parses*. Operator-configured at
  launch (never request-driven → Rule §3 holds); skipped on `dry_run`; surfaced in `vyer://status`
  (`verify=…`). (`engine.rs::run_verify` + commit arm; `EngineConfig.verify_cmd`; `main.rs --verify`.)
  3 integration tests (pass / fail / dry-run-skip). Layering: tree-sitter parse-gate catches *syntax*
  pre-write; `--verify` catches *semantic/type* errors (e.g. the non-exhaustive `match` that motivated
  this) post-write.
- **"Prefer vyer" MCP instructions.** Strengthened the server's `get_info` instructions so an agent is
  told to **use `code`/`code_apply` instead of native Read/Grep/Glob/Edit/Write for in-repo files, and
  fall back to native only when vyer can't serve** (path outside root, binary, running a command, or a
  vyer error). Advisory (model-facing guidance on connect), not a hard gate — for hard enforcement a
  project rule/hook is the lever. *(Done through vyer, dogfooding the symbol-replace path.)*

- **SCRY-036 — diff output polish.** `code_apply` now wraps each edit's unified diff in a fenced
  ` ```diff ` block with a `+N -M lines (path)` summary, so markdown-aware clients colorize the +/-
  lines. *Honest limit:* this is the most vyer can do toward Claude Code's rich diff panel — that
  widget is **client-side, reserved for the built-in `Edit`/`Write` tools** (recognized by name +
  schema) and is **not** something an MCP server's text output can trigger. A genuine UX edge native
  editing keeps. (`engine.rs::format_diff`; done through vyer.)

- **SCRY-037 — compact apply responses (real fix, found while building a Flutter app).** `code_apply`
  echoed the *entire* created file and full diffs back into the response — flooding the agent's context,
  the opposite of vyer's "compact output" principle, and the actual reason `Write` felt lighter than
  `@new` (the JSON-escaping cost is identical for both). Now: a create returns `new file PATH (+N lines)`
  + a 4-line head preview; edit diffs cap at 60 lines with a `… N more (truncated)` note. `@new` is now
  *strictly better than Write* for authoring — parse-validated **and** compact. (`engine.rs::creation_diff`,
  `format_diff`.)

Full suite **104 tests**, clippy clean.

### Thirteenth batch — C++/C#/PHP added **purely through vyer** + observations

Added **C++, C#, PHP** (`tree-sitter-cpp/c-sharp/php`) — **14 languages** now. Every source edit was a
vyer `code_apply`: anchored edits for the enum / `detect_lang` / match arms / `Cargo.toml` deps, an
atomic **cross-file** batch for `apply.rs` + `engine.rs`, and the `#@after:` insert directive for the
test. Only `cargo build`/`test` and grammar `tags.scm` lookups used Bash (verification/research, not
editing). 101 tests, clippy clean. Verified live (C++ `Widget`/`draw`/`main`, C# `Svc`/`Run`, PHP
`User`/`helper`).

**What worked well (pure vyer):**
- Module-level Rust edits — enum variants, match arms, `Cargo.toml` deps — via **file-scope anchored
  edits**; no full-file resend. The brace fix (SCRY-030) made Rust editable.
- **Atomic cross-file batch** committed `apply.rs` + `engine.rs` together (all-or-nothing) — a real win
  over native, where each `Edit` writes independently.
- `#@after:` cleanly **inserted a whole new test fn**.

**Issues / improvement areas observed (this session):**
- **SCRY-031 — `parse=ok` ≠ compiles (the big one).** vyer reported `parse=ok` on every edit, but only
  `cargo build` could confirm the `match` arms were *exhaustive* (`Lang::Php => &PHP`, etc.). A missing
  arm parses fine yet won't compile. vyer validates *syntax*, not *semantics/types*. **Fix = SP-8 full:
  an opt-in post-apply `cargo check` / project-test hook**, reported inline (`parse=ok; check=ok`).
  This is the single gap that made "pure vyer" unable to fully verify a Rust change.
- **SCRY-032 — `dump_ast` is referenced in error hints but unimplemented.** Authoring each tags query
  meant leaving vyer to read the grammar's `tags.scm` / probe node-kinds. A real `dump_ast`/`node_kinds`
  op (show a snippet's AST) would let an agent author AST/tags queries without leaving vyer.
- **SCRY-033 — anchors get long for big match arms.** A unique anchor sometimes needs lots of context
  (e.g. the whole `Lang::Rust | … | Lang::C => {` arm). A `PATH#SYMBOL` + `anchor` (symbol-scoped) or a
  short structural anchor would shorten them. (Symbol-scoped anchors exist; they don't help for
  free-function match arms.)
- **SCRY-034 (feature) — a language-pack scaffolder.** Adding a language is ~6 edits across 4 files in a
  fixed pattern; a higher-level `vyer add-language <name> --grammar <crate> --tags <q>` op could emit all
  the boilerplate in one call.

### Twelfth batch — C (added with native tools, for an A/B comparison)

Added a **C** pack (`tree-sitter-c`) — 11 languages now (Rust/Python/JS/TS/Go/Dart/Java/Ruby/Swift/
Kotlin/C). Done deliberately with **native Read/Edit** (not vyer) to compare the experience:

| | native (this C add) | vyer (anchored/atomic) |
|---|---|---|
| round-trips | Read + 8 separate `Edit` calls across 5 files | could be **1 atomic `code_apply`** (all edits) |
| safety | none — malformed code only caught at `cargo build`; a wrong edit #5 leaves #1–4 on disk (half-applied) | **parse-validated + all-or-nothing** rollback |
| anchoring | exact `old_string` match, often a prior `Read` per site | anchor text only, warm core always fresh |
| friction hit | `Edit` blocked by "file modified since read" (vyer had touched `Cargo.toml` out-of-band) | n/a (one freshness model) |
| build/test | needs `cargo` (Bash) | also needs `cargo` (Bash) — neither edits-tool runs the compiler |

Verdict: for a structured multi-file change, vyer's editing is lower-friction and safer (fewer
round-trips, atomic, syntax-gated); native is more universal/zero-setup but manual and riskier. Both
still need `cargo build`/`test` to verify. Full suite **100 tests**, clippy clean.

### Eleventh batch — Java/Ruby/Swift/Kotlin + a dogfooding bug fix (SCRY-030)

Added **four more language packs** — Java, Ruby, Swift, Kotlin — each ~6 lines (a grammar fn + a 3-line
tags query + one `pack()` arm + enum/`detect_lang`/`lang_extensions` entries). All extract
classes/methods/functions/interfaces/etc.; verified live on a polyglot Java+Ruby+Swift+Kotlin project.
Languages now: **Rust, Python, JS, TS, Go, Dart, Java, Ruby, Swift, Kotlin (10).**

**⚠️ SCRY-030 — real bug found by dogfooding (FIXED).** Trying to add the languages *through vyer*
surfaced it: the apply-path's `brace_balanced` pre-check treated `'` as a string delimiter, so Rust
**lifetimes** (`'static`, `'a`) and char-literals (`'{'`) desynced the bracket scan — vyer rejected
*every* anchored/deterministic edit to a real Rust file as "unbalanced `}`", **including its own
source.** Fixed: `brace_balanced` is now language-aware (`'` = char/lifetime in Rust/Go/Java/Kotlin/
Swift, a string in JS/TS/Dart, skipped for `end`-based Ruby), index-based with look-ahead to tell `'x'`
from `'static`. Regression tests added. Verified: vyer now anchored-edits its own `vyer-incr/lib.rs`.

> Honest note on method: the *editing* of vyer's Rust source this round was done with native tools
> because (a) vyer was mis-rooted at the snake playground, and (b) the running binary predated the
> SCRY-030 fix, so it literally could not edit Rust until the fix shipped + rebuilt. The `Cargo.toml`
> dependency edits **were** done through vyer (anchored, file-scope). Post-fix, vyer can edit Rust
> source itself — demonstrated live.

### Tenth batch — pluggable language architecture + Dart/Flutter (SCRY-029)

Replaced the hard-coded per-language extraction (scattered across `kind_of`/`collect`/`js_var_fn_name`/
`name_from_header`) with a **registry of language packs + one generic tree-sitter tags-query extractor**
(`vyer-index::langpack`). Each language is now a `LangPack` (a grammar fn + a `@def.<kind>`/`@name` tags
query) + one `pack()` arm — adding a language is ~6 lines, no engine/apply changes.

| | Result |
|---|---|
| 5 existing langs | migrated to the generic extractor; **Go-receiver & JS-arrow special cases deleted** (the grammar's own structure handles them) |
| **Dart/Flutter** | shipped as the first new pack (`tree-sitter-dart`); all 9 superpowers work — verified live (context + repo-wide rename on a Flutter widget) |
| isolation | per-**file** routing + **lazy `OnceLock`** grammar/query init → a Dart project never builds the Python/Rust grammar; **no cold-start regression** |
| mixed repos | a Python project with `.dart`/`.js`/`.css` files routes each file to its own pack; unsupported files (CSS) degrade to lexical search, never error |

Plan + as-built notes (incl. answers to the cold-start / mixed-language questions): `docs/language-architecture.md`.
Full suite **98 tests**, `clippy` clean.

### Ninth batch — one-call context pack (SP-9) + agent-decision eval

| ID | Change | Verified |
|---|---|---|
| **SP-9 ⭐** | **`detail=context`** — one call returns a symbol's definition (full) + callees ([calls]) + callers ([called by], tests tagged), budget-packed and edge-ordered. Replaces the 4–8 calls an agent makes to assemble this. | live: `validate_token` → def + `check_len` callee + 2 callers (1 test) |
| **agent_eval** | `benches/track_a/agent_eval.py` → `results/agent_eval.md`: measured tool-cost model over a 7-task corpus. vyer = **7 round-trips vs native 27+** (~3.9×); a budget-optimizing agent picks vyer **6/7**. | scorecard written |

`engine::context_spans`. Full suite **96 tests**, `clippy` clean. **Eight superpowers shipped
(SP-1…SP-7, SP-9)** + SP-8 partial.

---

## 10b. SCRY-038 — substring/prefix recall (HIGH, fixed)

**Symptom.** `mode=lexical q="confiden"` returned **0 matches** though `low_confidence` appears
repeatedly — a plain `grep confiden` finds it. **Cause.** The inverted index (`build_token_index` /
`identifiers`) keys on *whole* identifiers, and `pruned_lex_files` did an exact `postings.get(q)`;
a query that is a substring/prefix of larger identifiers (but not itself a standalone token, and not
a symbol name) pruned to an empty candidate set before any byte was scanned — silently dropping real
matches and undercutting recall (Rule §10). **Fix.** `postings_substring` unions the file lists of
*every* index key that contains the (lowercased) needle. Because a plain-identifier match always
lands inside a single identifier token, this is exactly the set of files that can match — **full
recall, still pruned** (no read of a non-candidate file). Applied to both the plain path
(`pruned_lex_files`) and the boolean AND path (`and_candidate_files`). Tests:
`lexical_substring_query_keeps_recall`, `boolean_and_substring_recall`.

## 10c. SP-12 — `@into:Container` insert (editing bottleneck)

**Gap.** Adding a method/field to an existing class/impl/struct forced `@after:<some member>`
(you had to know a member's name) or a manual edit; `@end` only appends at file end. **Added.**
`code_apply {locator:"PATH#@into:CONTAINER"}` splices `new_body` in at the right spot for the
language: **before the closing `}`** (Rust/JS/TS/Go/Java/Kotlin/Swift/C/C++/C#/Dart/PHP), **before
Ruby's `end`**, or **at the end of the indented body** (Python). Single-line blocks and unrecognized
languages are **refused** with an actionable hint (`@after:<member>`) rather than misplaced. An optional `@into:Name@Lstart-end` disambiguates
same-named blocks (a struct and its impl). Re-parse-gated like every edit, so a bad splice is
rejected, never written. (`apply.rs`: `InsertPos::Into`, `prepare_insert`; `engine.rs`: directive
parse with embedded `@L`.) Verified e2e: dry-run added a field before `EngineConfig`'s `}`.
Test `insert_into_container_before_closing_brace`.

## 10d. SP-13 — `detail=ast` (dump_ast affordance)

**Gap.** `mode=ast` runs tree-sitter S-expression queries, but you can't author `(class_definition …)`
without knowing the node kinds — the no-match hint even promised a `dump_ast` that didn't exist.
**Added.** `code {path:"a.py", detail:"ast"}` dumps the file's tree-sitter AST as an indented list of
NAMED node kinds with line ranges (anonymous punctuation/keywords skipped, capped at 400 nodes). The
agent reads it, then writes the `mode=ast` pattern. The `mode=ast` failure hint now points at it.
(`vyer-index`: `dump_ast`/`walk_ast`; `engine.rs`: `dump_ast_spans`, `detail=="ast"` branch.)
Verified e2e: dumped `mcp_client.py` → `(module) (import_statement name: (dotted_name (identifier)))…`
with **field-name edge labels** (`name:`/`body:`/…) so field-qualified queries
(`(class_definition name: (identifier) @c)`) are author-able too. An optional `lines` filter
(SP-13b) prunes to one construct's subtree (overlap test, line numbers preserved) — e2e: `--lines
13-20` on `mcp_client.py` dumped just the `MCPClient` class region. Test `dump_ast_lists_node_kinds`.

## 11. Superpower roadmap — beyond parity

The work above brought vyer to **parity-plus** with native tools and closed every CRITICAL/HIGH gap.
To make an agent feel *superpowered* — operations native `Read`/`Edit`/`Grep`/`sed` fundamentally
**cannot** do safely, because they have no warm AST + graph + apply core — here is the prioritized
roadmap. **SP-1 is shipped.**

- **SP-1 — Repo-wide symbol-aware rename (SCRY-027). ✅ DONE.** Atomic, parse-validated, cross-file.
  The flagship: a refactor an agent can trust in one call. **Bench: 1 call vs 14–44 native tool calls,
  ~9–20× fewer tokens.**
- **SP-2 — Atomic multi-edit transactions (SCRY-011). ✅ DONE.** *Every* `code_apply` batch is now
  all-or-nothing: edits update the warm core per-edit (intra-batch freshness) but disk writes are
  **deferred**; on any failure the warm core is rolled back and **no file is touched**; only on full
  success are the buffered writes flushed. (`engine.rs`: `DiskOp`, `snapshot`, the commit/rollback
  match.) **Bench: a 3-edit batch with one bad edit → 0 files changed; native Edit leaves the repo
  half-edited.**
- **SP-3 — Impact / blast-radius (`detail=impact`). ✅ DONE.** Transitive referrers in one call —
  "if I change `X`, what breaks?" walks the word-boundary ref approximation breadth-first (depth-capped,
  honestly `graph=partial`). (`engine.rs`: `impact_spans`, `contains_word`.) Native = dozens of greps +
  manual tracing.
- **SP-4 — Bulk structural search-and-replace. ✅ DONE.** A glob locator (`src/**`, `**/*.py`) with an
  `anchor`/`replace` rewrites *every* occurrence in *every* matching file — parse-validated per file and
  committed all-or-nothing. `sed` across a repo, with a safety net. (`engine.rs`: the bulk branch,
  `replace_all_str`.) Live: 5 occurrences across 2 files in one call.
- **SP-5 — Move symbol across files. ✅ DONE.** `code_apply {locator:"A#sym", move_to:"B"}` cuts the
  symbol from A and appends it to B (creating B if needed), re-parses both, commits atomically.
  (`engine.rs`: the move branch; `apply::symbol_text`.) Live: `helper` moved A→B, searchable in B.
- **SP-6 — Edit history / undo. ✅ DONE.** Each successful `code_apply` batch snapshots the pre-edit
  text of every file it touched onto a history stack; `code_apply {undo: N}` reverts the last N batches
  (restores content, recreates deleted files, deletes created ones) on disk **and** in the warm core.
  Safe exploration: an agent can try a refactor and cleanly roll it back. (`engine.rs`: `history`,
  `EditBatch`, the undo branch.)
- **SP-7 — AST-pattern + semantic search (SCRY-016 / SCRY-022). ✅ DONE.** Two complementary modes:
  - **`mode=ast`** runs a real tree-sitter S-expression query (`(call function: (identifier) @fn)`,
    `(class_definition …)`) → matched node spans. Structural search `grep` can't do; invalid queries
    reported, not silent (closes SCRY-016). (`vyer-index::ast_query`, `engine::ast_spans`.)
  - **`mode=semantic`** is now real: **deterministic, zero-dependency subword TF-IDF**. It splits
    camelCase/snake_case and ranks by query-term IDF overlap, so "check whether the auth token is valid"
    → `validate_token` *without* the exact name. (`engine::semantic_spans`, `subword_tokens`.)
    **Honest boundary:** it's lexical-subword, not neural — it nails vocabulary-overlap queries and
    *honestly returns nothing* for pure-synonym queries ("draw"≠"render"). A neural embedding reranker
    would add true synonymy; that (a model download) is the only optional enhancement left, and the
    interface is already in place to drop it in.
- **SP-8 — Post-apply semantic gate (SCRY-010/018).** Optional import/typecheck/test run after an edit,
  reported inline (`parse=ok; pytest=12/12`). **Partial ✅:** rename now rejects a target that is a
  reserved **keyword** (`is_language_keyword`), closing the lenient-tree-sitter hole for the rename
  path. Full per-language typecheck/test pass remains.
- **SP-11 — Subtree outline (`detail=outline`, no `q`/`path`). ✅ DONE.** One call returns the
  symbol map (signatures, bodies elided) of every file in scope — the "lay of the land for this
  directory" an agent reads to orient, replacing N per-file reads. Scope with `path_scope` (incl.
  `!` exclusions) and/or `lang`. (`engine.rs`: `outline_spans`.)
- **SP-10 — Session diff (`detail=diff`). ✅ DONE.** One call returns every change an agent
  has made to the repo this session, as a fenced unified diff per file. Built from the SP-6
  history snapshots (session-original text) diffed against the current warm core — deterministic,
  in-process, **no `git`**. `q` (path substring) and `path_scope` (globs) filter it; net-zero
  files (edited then reverted/undone) are omitted. The "what have I changed so far?" an agent
  otherwise reconstructs by hand. (`engine.rs`: `diff_spans`; reuses `apply::line_diff`.)

> **Note:** the live MCP server in `.mcp.json` points at `target/release/vyer`; it must be **restarted**
> to pick up these changes (a long-running daemon keeps the old binary in memory).

**Remaining work** (all the headline gaps are now closed): SCRY-005/006 class-qualified locators +
apply miss diagnostics · SCRY-016/017 structural AST-pattern search / class-member outline ·
SCRY-010/018 optional post-apply semantic (import/typecheck) gate · SCRY-011 atomic multi-edit
transactions · SCRY-007/012/019/022/023(done)/024/025 search/DX polish. These are medium/low; the
critical-and-high register is essentially cleared.

---

## 7. Evidence appendix

**Task replay (commenting `playground/snake`).**
- ✅ `game.py` (11), `ai.py` (6), `input.py` (5), `render.py` (14), `main.py` (3), `test_game.py` (12)
  symbols commented via `code_apply`, each `parse=ok`; `python3 test_game.py` → **12/12 passed**.
- ⚠️ `main.py#_title_screen` ASCII-banner space dropped on full-body resend → SCRY-004.
- ⛔ `highscore.py`, `snake.py`, `board.py`, `food.py` + all module-level lines done via native `Write`
  → SCRY-002.

**Bench reproduce.**
```sh
python3 benches/track_a/run_track_a.py     # read+search → results/track_a_xs.csv
python3 benches/track_a/edit_bench.py       # edit/freshness/E5 → results/track_a_edit_xs.csv
# full method + tables: results/report.md ; plan + next tiers: benchmark-plan.md
```

**Not yet measured (open work).** Track A at S/M/L repo scale (warm-core gap should widen); "round-trips
to answer" for search (grep often needs grep→Read→grep; vyer one call); **Track B** agent-level
vyer-only vs native-only outcomes (the metric that ultimately decides this).
