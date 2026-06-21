# The 2026 Code-Context Engine — v5: The Incremental Core, Now Real
### Supplements v4. The architectural centerpiece — the incremental warm core with read-after-write freshness — is now written, compiling, and proven by test, not described.

> v4 remains the full design (evidence, landscape, interface spec, `auto` routing, security, evaluation, positioning). **v5 replaces v4's §9–§14** with the now-*working* reality: the incremental warm core is implemented in `ccx-incr`, runs end-to-end, and its key property — *editing one file recomputes only that file's queries* — is demonstrated with measured recompute counters. The reference workspace now has **3 crates and 18 passing tests**.

---

## 1. The mechanism, now demonstrated

The design's hardest claim has been: a Salsa-style incremental core gives the agent a *warm* code model where every edit invalidates only the affected slice, so retrieval is fast and a read right after a write is always fresh. v5 makes that real and observable.

`ccx-incr` models file contents as **inputs** and `parse` → `symbols` → `outline` → `repo_outline` as **derived, memoized queries**. `set_text` bumps a global revision (unchanged text is a no-op — "durability"); a derived result is reused iff the content hash it was computed from still matches the input's current hash. Recompute counters (`Stats`) make selective recomputation testable.

**Actual demo output** (`cargo run -p ccx-server`):
```
# incremental warm core (read-after-write freshness)
after first repo_outline:  parses=2 symbols=2 outlines=2 repo_builds=1
token.rs outline: ["fn validate_token(tok: &str) -> Result<Claims> @L1-3"]

-- edited token.rs (added `refresh`); login.rs untouched --
after second repo_outline: parses=3 symbols=3 outlines=3 repo_builds=2
   delta: parses +1 symbols +1 outlines +1 repo_builds +1  (only token.rs recomputed)
token.rs outline now: ["fn validate_token(...) @L1-3", "fn refresh(...) @L4-6"]

✓ only the edited file's chain recomputed; new symbol visible immediately.
```

Two files are outlined (2 parses, 2 symbol-extractions, 2 outlines). After editing **only** `token.rs`, re-outlining the whole repo costs exactly **+1** of each — `login.rs` is served from memo — and the new `refresh` symbol is visible on the very next query. That is read-after-write freshness with selective invalidation, measured.

The property is locked in by tests, not just the demo:
- `edit_recomputes_only_that_files_chain` — after warming `a.rs` and `b.rs`, editing `a.rs` and then querying `b.rs` recomputes **nothing**; querying `a.rs` recomputes its chain exactly once.
- `unchanged_set_text_does_not_bump_revision_or_recompute` — durability.
- `repo_outline_rebuilds_only_when_a_file_changes_and_reuses_unchanged` — the repo-level query rebuilds once on change and reuses every unchanged file's outline underneath.
- `symbols_are_extracted_from_real_rust`, `python_and_js_extract_too` — the extractor works on real source across three languages.

---

## 2. Updated reference-implementation map (replaces v4 §9)

`ccx-reference.zip` — a real Cargo workspace, **3 crates, 18 passing tests**, all offline-buildable:

```
ccx/
├── crates/ccx-core/        # pure logic, ZERO deps — 11 tests
│   └── src/lib.rs          #   locator · fusion(RRF) · budget · ordering(LiM) · sandbox · output
├── crates/ccx-incr/        # incremental warm core, ZERO deps — 7 tests   ← NEW in v5
│   └── src/lib.rs          #   Db{ set_text, parse, symbols, outline, repo_outline } + extractor + Stats
├── crates/ccx-server/
│   ├── src/main.rs         # demo: retrieval pipeline + the incremental freshness demo above
│   └── src/rmcp_server.rs.reference   # real rmcp `code` + sandboxed `code_apply`
└── README.md
```
`cargo test` → 11 + 7 pass. `cargo run -p ccx-server` → retrieval envelope **and** the freshness demo.

**What's real vs next (honest status):**

| Component | Status |
|---|---|
| Locators, RRF fusion, budget packing, lost-in-the-middle ordering, write sandbox, UNTRUSTED output | **real + tested** (`ccx-core`) |
| Incremental query engine: memoization, revision/durability, selective invalidation, read-after-write freshness, recompute telemetry | **real + tested** (`ccx-incr`) |
| Symbol/outline extraction for Rust/Python/JS | **real + tested** (modest scanner; tree-sitter swaps in at the same `parse`/`symbols` interface) |
| MCP front end (`code`, `code_apply`) over `rmcp` | **real reference** (drop-in; needs the `rmcp` dep) |
| Trigram/BM25 index, the apply merge, the graph layer, the contextual semantic mode | **stubbed at marked call sites** — the next build steps |

---

## 3. `ccx-incr` walkthrough (replaces v4 §10's incremental portion)

- **`Db`** — holds inputs (`path → {text, hash, lang}`), a `revision`, one memo table per derived query, and `Stats`. Queries take `&self` (interior-mutable memo tables, like Salsa); `set_text` takes `&mut self` (the only input mutation).
- **`set_text(path, text)`** — hashes the text; if unchanged, returns without bumping `revision` (durability); else updates the input and bumps. Synchronous, so a subsequent query sees the new value (freshness).
- **`parse(path)`** — the structural step (item boundaries). Memo hit iff the file's content hash matches; else recompute + `stats.parses += 1`. *(tree-sitter goes here in production.)*
- **`symbols(path)`** — depends on `parse`; extracts names + signatures. Memo-validated by content hash.
- **`outline(path)`** — depends on `symbols`; signatures-only, bodies elided (the cheap `detail=outline` view).
- **`repo_outline()`** — depends on the multiset of all file hashes; reused unless some file changed, and when one changes only that file's `outline` recomputes underneath (others hit memos). This is the repo-map data path.
- **`Stats`** — recompute counters per query, exposed for tests *and* as live telemetry (v5 §6).

The extractor (`mod lang`) is deliberately modest but real: brace-matching item detection for Rust/JS, indentation-based for Python, then name/signature cleanup. It's honest about its limits — strings/comments aren't tokenizer-correct — with the explicit note that tree-sitter replaces it at the identical interface. The *incremental mechanism* around it is the production one.

---

## 4. The freshness guarantee, validated (replaces v4 §6.x freshness claim)

v4 asserted "read-after-write staleness = 0 (hard requirement)." v5 demonstrates it: `set_text` mutates the input before the next query resolves, and the memo-by-content-hash design means the prior result is structurally invalid the instant the hash changes — so the next `parse`/`symbols`/`outline`/`repo_outline` recomputes from the new text. The `unchanged_set_text…` and `edit_recomputes_only…` tests pin both halves: no spurious recompute on a no-op write, and a guaranteed fresh recompute on a real edit. In the full engine the same `set_text` is what `code_apply` calls synchronously after a write, and what the `notify` filesystem watcher calls on external edits — so the agent never reads stale code it (or the user) just changed.

---

## 5. Updated value model (replaces v4 §12's apply/freshness rows)

v4 estimated the freshness/incrementality win; v5 measures the mechanism:
- **Selective recompute:** on a 2-file edit, re-deriving the repo outline costs **+1** parse/symbols/outline instead of recomputing every file. Generalised: an edit touches **O(1) files' chains**, not **O(repo)** — the difference between a sub-50ms re-index and a full rescan on every keystroke/agent-write. (This is precisely why rust-analyzer stays responsive on huge projects; `ccx-incr` is the same mechanism in miniature.)
- **No stale-read tax:** because freshness is structural (hash-keyed memos), there is no 5-minute sync window and no "the agent edited a file then searched and got the old version" failure class — a real, recurring cost in index-based tools that this design eliminates by construction.

The apply-step numbers (deterministic AST splice ≪ frontier rewrite) and round-trip numbers (one batched `code` call vs sequential grep→read) from v4 §12 stand; v5 adds that the *index-maintenance* cost behind them is now demonstrably O(changed files).

---

## 6. Updated observability (replaces v4 §14's telemetry list)

The recompute counters aren't just for tests — they are the engine's core health signal. Expose `Stats` (and cache hit/miss derived from it) via the `ccx://status` MCP **Resource**: recomputes per query type, memo hit-rate, current revision, files pending re-index, and max staleness (which should stay 0). A rising recompute-to-query ratio is the canary for an invalidation bug or a thrashing watcher; a healthy engine shows near-zero recomputes on repeated reads (the `repeated_query_is_a_memo_hit` invariant, in production). Determinism (v4 §14) still holds: every query exercised here is deterministic, so identical inputs yield identical outlines and identical recompute behaviour.

---

## 7. Next build step (updates v4 §18 phase 2→3 boundary)

The incremental core (phase 2) now exists. The next increments, in order, each attach at a marked call site:
1. **Swap tree-sitter into `parse`** — replace the scanner with real grammars (100+ languages); the memoization, freshness, and `symbols`/`outline` layers above it are unchanged.
2. **Add the lexical index as a derived query** — a trigram/BM25 index (`tantivy` or positional-trigram) keyed off the same inputs, invalidated by the same hashes; fuse with structural via the RRF already in `ccx-core`.
3. **Wire the apply path** — deterministic AST-anchored splice (validated by re-parse) writing through `set_text` for synchronous freshness; the sandbox is already in `ccx-core`.
4. **Graph + optional contextual semantic mode** — as in v4 §18.6, behind the same `Db` and the same tiny tool surface.

Each step is additive and independently testable, because the incremental spine and the deterministic core are already in place.

---

## 8. Pointer to the full design

For everything unchanged — the benchmark evidence, the three-modality framing, the saturated-landscape analysis, the single-`code`-tool interface spec, the precise `auto` routing algorithm, the tool-economy / code-execution-with-MCP economics, the security threat model and defenses, the evaluation methodology, the multi-language degradation matrix, and the positioning/distribution strategy — see **v4**. v5 changes only the engineering reality of the core: it's no longer a sketch.

---

*Reference workspace: 3 crates, 18 tests passing as of writing (`cargo test`). Integration components (tree-sitter, the index, the apply merge, the graph layer, the semantic mode) remain stubbed at the marked call sites; the incremental spine, the deterministic logic, and the security sandbox are real and tested. Figures elsewhere are directional — re-benchmark on your own workload.*
