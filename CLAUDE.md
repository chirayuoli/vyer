# CLAUDE.md — Vyer

> **Vyer** is a resident, local-first **code-context engine for AI agents**: one MCP tool that
> unifies lexical + structural + graph search over a repo, returns compact best-at-the-edges spans,
> and closes the loop into a sandboxed apply path — served from an incremental warm core so a read
> right after a write is always fresh.
>
> This file is the build contract. Read it fully before writing code. Two companion design docs
> hold the evidence and full rationale: `docs/design-v4.md` (full design) and `docs/design-v5.md`
> (the working incremental core). When in doubt, this file wins; if this file is silent, the docs win.

---

## 0. The one thing to remember

**Do not build "another semantic code-search MCP server." That lane is saturated.** Vyer's value is the
*integrated loop*: hybrid retrieval + a deterministic/fast apply path + an incremental warm core with
read-after-write freshness + a tiny tool footprint + secure-by-default. Every task you do should serve
that loop. If a change makes Vyer more like a generic grep wrapper, stop.

---

## 1. Non-negotiable rules (each is evidence-backed; violating one is a regression)

1. **Tiny tool surface.** Expose **one** MCP tool, `code` (+ a gated `code_apply`). Never add a third
   tool without removing one. *Why: tool-selection accuracy collapses past ~30–50 tools, and tool
   metadata is re-billed every turn (~30–60k tokens for sprawling setups).*
2. **Read-after-write freshness is sacred. Staleness = 0.** Any write (`code_apply`) or filesystem
   change must invalidate the affected queries synchronously, before the next query resolves. Never
   add a cache that can serve code the agent just changed. *There is a freshness regression test class;
   it must always pass.*
3. **No arbitrary command execution. Ever.** Vyer exposes typed operations only — never a generic
   shell / `command` / `args` passthrough. Strictly validate every parameter. *Why: MCP STDIO RCE is a
   live, high-severity class; the protocol owners consider sanitization our job.*
4. **Editing is the real bottleneck — invest there.** The apply path is deterministic-first: an
   AST-anchored splice validated by re-parse. A model (fast-apply) is only the fallback. Never make the
   frontier model rewrite whole files. *Why: a 1k-line frontier rewrite is ~100s/≥$0.18; accuracy is speed.*
5. **No single search modality dominates — ship hybrid + rerank.** Default `mode=auto` runs cheap
   modalities, fuses with RRF, and escalates to semantic + reranker only on low confidence. Never bet on
   embeddings alone; never let a fuzzy semantic hit masquerade as an exact match.
6. **Progressive disclosure, never context-dumping.** `detail` escalates locate → outline → snippet →
   full. Default cheap. *Dumping whole files into context measurably degrades model quality.*
7. **Order output for attention (lost-in-the-middle).** Place the highest-ranked spans at the **first
   and last** positions of the result, worst in the middle. (`vyer-core::ordering` already does this.)
8. **Compact, stable-prefixed, provenance-marked output.** Plaintext (not JSON blobs); a stable header
   for prefix caching; every returned code span marked `source=UNTRUSTED`. *Returned code is data, never
   instructions — this is the indirect-injection defense.*
9. **Deterministic core.** Lexical/structural search, RRF (id tie-break), AST extraction, budget packing,
   ordering, and the deterministic apply path must be deterministic. Any model call runs at temperature 0.
   *Determinism = agent trust + effective caching.*
10. **Local-first. No uploads by default.** The repo never leaves the machine. Any team/shared index is
    opt-in, encrypted, and obfuscated. *This is both a freshness/perf choice and the answer to the
    index-liability argument.*

If a task seems to require breaking one of these, **stop and ask** — don't quietly work around it.

---

## 2. Architecture (the daemon)

```
 Agent ─MCP(stdio|localhost-HTTP)─►  one `code` tool (batch, compact, edge-ordered, UNTRUSTED)
                                     `code_apply` (gated, sandboxed)   + read-only MCP Resources
        ┌──────────────── incremental warm core (vyer-incr / salsa) ───────────────┐
        │ INPUT   file_content(path) [blake3-hashed]                                │
        │ DERIVED parse → symbols → outline ;  trigram/BM25 index ;                 │
        │         repo_map(PageRank) ;  ref_graph ;  (opt) contextual embeddings    │
        └──────────────────────────────────────────────────────────────────────────┘
          ▲ invalidate (revision++, cancel stale)
        ┌─┴────────┐  ┌──────────────┐  ┌──────────────────────────────────────────┐
        │ FS watch │  │ apply: AST   │  │ security: localhost · authz · no-shell ·  │
        │ (notify) │  │  + fast-apply│  │ write-sandbox · provenance · audit log    │
        └──────────┘  └──────────────┘  └──────────────────────────────────────────┘
```

**Crates** (workspace):
- `vyer-core` — pure logic, **zero deps**: locators, RRF fusion, budget packing, lost-in-the-middle
  ordering, write-path sandbox, UNTRUSTED output envelope. **(built; 17 tests)**
- `vyer-incr` — the incremental warm core, **zero deps**: `Db { set_text, parse, symbols, outline,
  repo_outline }` with memoization, revision/durability, selective invalidation, recompute telemetry,
  and a modest real symbol extractor (Rust/Python/JS). **(built; 13 tests)**
- `vyer-server` — the MCP front end (`rmcp`) + a CLI. **(reference exists; to flesh out)**
- `vyer-index` *(to create)* — tree-sitter parsing, the trigram/BM25 index, the graph layer, the
  optional semantic mode. Each attaches at a `vyer-incr` derived-query call site.

> The current scaffold uses crate prefix `ccx-*` as a placeholder. **Rename `ccx-core → vyer-core`,
> `ccx-incr → vyer-incr`, `ccx-server → vyer-server`** (directory names + `name =` in each Cargo.toml +
> the workspace members + `use` paths) as the first task, unless the project is kept as `ccx`.

---

## 3. Current state — what's real vs what to build

| Component | Status |
|---|---|
| Locators, RRF, budget packing, lost-in-the-middle ordering, write sandbox, UNTRUSTED output | **real + tested** (`vyer-core`) |
| Incremental engine: memoization, durability, selective invalidation, read-after-write freshness, recompute telemetry | **real + tested** (`vyer-incr`) |
| Symbol/outline extraction — heuristic scanner (fallback) **and** real **tree-sitter** parsing (Rust/Python/JS/TS/TSX/Go/Dart/Java/Ruby/Swift/Kotlin/C/C++/C#/PHP — 14 langs) | **real + tested** (`vyer-incr` + `vyer-index`); injected at the zero-dep parser hook, incremental spine untouched |
| MCP `code` / `code_apply` over `rmcp` (stdio) + localhost-token HTTP; lexical(+inverted index, substring-recall)+structural+graph+semantic-escalation+ast search; full detail surface (locate/outline/snippet/full/refs/impact/context/count/tree/diff/ast; `!`-exclusions, multi-lang, boolean); audit log; gated/sandboxed apply (new_body/anchor/rename/move/insert/@into/@delete/undo) | **real + tested** (`vyer-server`) — Phases 1–5 + Phase-7 polish; 163 server tests / 207 workspace incl. real stdio-subprocess e2e (tools/resources, write path + diff, write-gate + sandbox red-team), a CLI smoke test, and a live HTTP round-trip |
| Deterministic AST-anchored apply: symbol-splice, re-parse validation, unified diff, synchronous freshness | **real + tested** (`vyer-server::apply`) |
| Repo-map (PageRank over the reference graph) + read-only MCP **Resources** (`vyer://repo-map`, `vyer://status`, `vyer://playbook`) | **real + tested** (`vyer-core::repomap` + both transports) |
| Graph `detail=refs` — definition + cross-file references, honestly tagged `graph=partial(approx)` | **real + tested** (lexical/tree-sitter approximation; LSP sidecar is the future upgrade) |
| FS watcher + `reindex` so out-of-band edits become query-ready; build/vendor-dir pruning; token inverted index | **real + tested** (`vyer-server::watch`, `Engine::reindex_*`) |
| Tantivy/BM25-at-huge-scale, fast-apply **model** fallback, full **LSP** graph, contextual **semantic** embeddings | **not yet** — Phase 6 + the heavy-dependency tail (need model/LSP downloads); interfaces + honest degradation are in place |

Proven invariants (keep them green): editing one file recomputes only that file's chain;
unchanged `set_text` is a no-op; `repo_outline` reuses unchanged files; the write sandbox rejects
escapes / `mcp.json` / `.git/hooks`; a query right after `code_apply` returns the fresh body
(staleness = 0); writes are refused unless `--allow-writes`; HTTP refuses any non-loopback bind and
requires a bearer token; tree-sitter node spans drive snippet/apply; no-match returns an actionable
`code/error` envelope. **Warm-query SLO validated empirically** (`cargo run -p vyer-server --example
warm_bench --release`, which now also exercises the graph modes): p50 ≈ 3 ms, p95 ≈ 8 ms —
search *and* the graph modes (refs/context) alike on this ~159-file repo (comfortably within
the p50<30ms / p95<120ms SLO; multi-word literal phrases are inverted-index pruned — SCRY-047;
refs and context's caller scan are file-pruned — SCRY-061/062, which halved graph p95 from
≈18ms; figures grow with repo size and are directional).

> **Toolchain:** the workspace builds on **stable Rust** (pinned via `rust-toolchain.toml`) because
> `rmcp`/`ignore`/`grep-*`/`tree-sitter` require a recent edition. `vyer-core` and `vyer-incr` remain
> zero-dependency; the dependency surface (`rmcp`/`tokio`/`ignore`/`grep-*`/`notify`/`serde`/`schemars`)
> lives in `vyer-server`, and `tree-sitter` in `vyer-index` — both at the edge.

---

## 4. Build roadmap (do these in order; each is additive and independently testable)

**Phase 1 — Secure MCP server, one `code` tool (lexical only).**
- Flesh out `vyer-server` from `rmcp_server.rs.reference`: `code` tool wired to lexical search using the
  ripgrep libraries (`grep`, `grep-searcher`, `ignore` crates — **not** by shelling out). `detail`:
  `locate`/`snippet`/`full`. Output via `vyer-core::output` (compact, edge-ordered, UNTRUSTED, budgeted).
- Security: stdio by default; HTTP only on `127.0.0.1` with a bearer token; typed params; audit-log every
  call; `code_apply` gated behind `--allow-writes` and sandboxed via `vyer-core::sandbox`.
- **Done when:** an integration test starts the server, runs `code` against a fixture repo, and asserts a
  well-formed envelope; a red-team test confirms no path escape and no command execution; `cargo clippy`
  is clean.

**Phase 2 — Real parsing (tree-sitter into `parse`).**
- Create `vyer-index`; replace the scanner in `vyer-incr::lang` with `tree-sitter` + grammars (start:
  Rust, TS/JS, Python, Go). Keep the exact `parse`/`symbols`/`outline` signatures so the incremental
  spine is untouched. `snippet` now returns the enclosing AST node.
- **Done when:** the existing `vyer-incr` freshness tests still pass against real parsing; outline/snippet
  use real nodes; a "huge/binary/unparseable file" robustness test passes (degrade, don't crash).

**Phase 3 — Lexical index at scale + fusion + repo map.**
- Add a trigram/BM25 index (prefer `tantivy`; or a positional-trigram index) as a `vyer-incr` **derived
  query** keyed off the same inputs and invalidated by the same hashes. Fuse lexical + structural via
  `vyer-core::fusion` (RRF, weights ≈ 0.8 structural/lexical, 0.2 semantic). Add a PageRank repo map
  (token-budgeted) exposed as an MCP **Resource**.
- **Done when:** a SWE-Explore-style retrieval eval harness runs; scale test on a large repo meets the
  SLOs (§7); fusion provably combines modalities.

**Phase 4 — Apply path.**
- Deterministic AST-anchored splice: locate the symbol's node, replace its text with `new_body`,
  **re-parse to validate** (reject if it doesn't parse — no silent bad write), write, then `set_text`
  synchronously (freshness). Then the optional `lazy_edit` → fast-apply model fallback (temperature 0;
  reject on parse failure).
- **Done when:** edit success / reject / well-formed metrics are tracked; a freshness regression test
  (write then immediately query → fresh) passes; sandbox holds.

**Phase 5 — Graph layer (`refs`).**
- LSP multiplexer where a server exists (tier-1 langs); `stack-graphs`/tree-sitter approximations
  elsewhere. Add `detail=refs` (callers/callees/impls). Treat LSP as a best-effort sidecar: on crash,
  degrade and report `graph=degraded`. **Report the tier** (`full|partial|none`) so the agent calibrates.
- **Done when:** go-to-def/find-refs work for tier-1 langs; graceful degradation test passes.

**Phase 6 — Optional contextual semantic mode (opt-in, off by default).**
- tree-sitter chunking on semantic boundaries → per-chunk contextual header generated by a **small local
  model** (cached by content hash; this is the only non-deterministic step) → index for both embedding
  and contextual-BM25. Embed with **BGE-M3** (MIT; dense+sparse in one) + **BGE-reranker-v2**, or
  **Qwen3-Embedding/Reranker 0.6B**, or **embeddinggemma**/`nomic-embed-text` for the lightest footprint.
  int8 quantization + content-addressed cache + `usearch`/`arroy`. Retrieve ~150 → rerank → ~20. Strictly
  opt-in; clearly labeled; **never masquerades as exact**.
- **Done when:** a Pass@k code-retrieval eval shows the contextual+rerank lift; ablation confirms it helps
  the "I don't know the symbol name" case.

**Phase 7 — Footprint + polish.**
- Code-execution surface (present Vyer as a small code API for harnesses with code execution, so
  intermediate results stay out of context). Read-only MCP Resources (`vyer://repo-map`, `vyer://status`, `vyer://playbook`).
  Compact-output polish. Optional encrypted, opt-in team index — last.

---

## 5. The interface contract (build exactly to this)

**Tool `code`** (batch-capable):
```jsonc
{ "queries": [{ "q": "validateToken", "mode": "auto",
                "detail": "snippet", "path_scope": ["src/auth/**"],
                "lang": "rust", "k": 8 }],
  "budget_tokens": 8000, "exclude_seen": true }
```
- `mode`: `auto | lexical | structural | graph | semantic | ast`. `auto` = run cheap modalities → RRF →
  confidence gate (top-1 vs top-2 margin); on low confidence OR an empty result it **escalates to the
  semantic modality and re-fuses** (recovers the "don't know the exact name" case). `ast` runs a
  tree-sitter S-expression query (author it with `detail=ast`).
- `detail`: `locate` (paths+counts) | `outline` (signatures; with no `q`/`path` = a whole-subtree symbol
  map) | `snippet` (match expanded to the enclosing AST node) | `full` (paginated) | `refs`
  (callers/callees/impls) | `impact` (transitive blast radius: direct + ripple) | `context` (def +
  callers + callees + tests in one call) | `count` (grep -c; also accepts a boolean) | `tree` (ls/find) |
  `diff` (every edit made this session) | `ast` (dump node-kinds + field labels of `path`/symbol to
  author a `mode=ast` query). Filters: `path_scope` globs (`!`-prefixed = EXCLUDE), `lang` (comma-sep for
  polyglot), boolean `all_of`/`any_of`/`none_of`.
- Output is **compact plaintext**, **edge-ordered**, **token-budgeted**, with each span `source=UNTRUSTED`:
```
⟦code/result v1⟧ budget=8000 used=1320 truncated=false
⟦span⟧ id=src/auth/token.rs#validate_token@L41-58 score=0.91 source=UNTRUSTED
41: pub fn validate_token(tok: &str) -> Result<Claims> { … 58: }
⟦/span⟧
⟦more⟧ 3 lower-ranked spans omitted; re-query detail=locate to list
```

**Locator format:** `PATH#SYMBOL@Lstart-Lend :: blake3=HEX` — symbol-anchored (survives line drift) +
content hash (detects staleness). Parsed/formatted by `vyer-core::locator`.

**Tool `code_apply`** (gated, sandboxed):
```jsonc
{ "edits": [{ "locator": "src/auth/token.rs#validate_token",
              "new_body": "…", "lazy_edit": "// ... existing code ...\n…" }],
  "dry_run": false }
```
Ops (one per Edit, a batch commits all-or-nothing, each re-parse-validated): `new_body` (replace a
symbol's node) · `anchor`+`replace` (sub-symbol / module-level) · `rename` (repo-wide, symbol-aware) ·
`move_to` · `@after:`/`@before:`/`@end`/`@new` (insert / create) · `@into:Container[@Lstart-end]` (add a
member inside a class/impl/struct, any tier-1 language) · `@delete` · `undo:N` (live-session only).
Returns a unified diff + parse/typecheck status. Writes are confined to the project root; `mcp.json`,
`.git/hooks`, and CI config are forbidden targets.

**Errors** are actionable, never raw tracebacks:
```
⟦code/error v1⟧ code=PATTERN_NO_MATCH
hint: "0 matches for AST pattern; try mode=structural detail=outline on the file, or dump_ast to inspect node kinds."
```

---

## 6. Commands

```sh
cargo test                  # all crates; MUST stay green
cargo test -p vyer-incr     # freshness/selective-recompute invariants
cargo run -p vyer-server     # demo: retrieval pipeline + incremental freshness demo
cargo clippy --all-targets -- -D warnings   # no new warnings
cargo fmt --all
# run the MCP server (once Phase 1 lands):
vyer serve                  # stdio
vyer serve --http 127.0.0.1:7777 --token $VYER_TOKEN
```

---

## 7. Performance SLOs (design against these; validate empirically)

| Op (warm, 10–50k-file repo) | Target |
|---|---|
| `locate`/`outline` query | p50 < 30 ms, p95 < 120 ms |
| `snippet` (AST-expanded) | p50 < 50 ms |
| `refs` (graph, where available) | p50 < 150 ms |
| Incremental re-index / edited file | < 50 ms to query-ready (lexical/structural/token: ✓ ~18 ms @50k. **Caveat:** the *opt-in* `semantic` mode rebuilds its tf-idf corpus on edit — ~125 ms @50k, bound by the global df/postings construction, not tokenization; a cached-bags attempt (SCRY-096) measured *worse* and was reverted. Off by default, so the default path meets the SLO.) |
| `apply` deterministic / fast-apply | p50 < 30 ms / < 1 s for ~500 lines |
| **Read-after-write staleness** | **0 (hard requirement)** |
| Tool-metadata footprint | < ~2k tokens (1 tool); ~0 via code-exec/Resources |

### 7a. Super-primitives vs coreutils (warm; `cargo run -p vyer-server --example primitives_bench --release -- crates`)

The warm core replaces the bash search/read tools and beats them because the data is
resident (no `fork`/`exec`, no cold `open`/`read`, no full rescan) and pre-indexed
(memoized line-offset index; inverted token index; sorted resident file set). Measured on
this repo's `crates/` tree:

| primitive (`code` param) | replaces | vyer warm p50 | coreutil p50 | speedup |
|---|---|---|---|---|
| `path`+`lines` (`40-80` / `-80` head / `~20` tail) | `sed -n` / `head` / `tail` | ~0.09 ms | ~2.2 ms | ~23× |
| `detail=count` | `grep -c` / `wc -l` | ~0.42 ms | ~4.7 ms | ~11× |
| `detail=tree` | `find` / `ls -R` / `tree` | ~0.006 ms | ~2.4 ms | ~400× |
| `all_of`/`any_of`/`none_of` | chained `grep` AND/OR/NOT | single AC pass + index pruning | N greps | — |

**Honest caveat (do not cherry-pick):** these are *warm* numbers — the resident daemon's
steady state. A one-shot `vyer` CLI call must index the repo first and will *lose* to grep
on a small tree; the win is the resident core + structure + freshness, not a faster regex.
The benchmark prints Vyer's one-time index cost up front for exactly this reason.

---

## 8. Coding standards

- **Rust, stable.** Workspace; small focused crates. `vyer-core` and `vyer-incr` stay **zero-dependency**.
- **No panics in the query path.** Malformed input → an actionable error envelope, never a panic/unwrap.
  `unwrap`/`expect` only in tests or provably-infallible spots (justify with a comment).
- **Errors:** typed (`thiserror`), with hints. Surface degradations (`graph=degraded`, `partial`), don't fail.
- **Tests are mandatory** for every non-trivial pure-logic piece, and for every freshness/security
  invariant. New behavior ships with tests. Property tests welcome for fusion/packing/ordering.
- **Determinism:** see Rule 9. If something must be non-deterministic (contextual headers), cache it by
  content hash and isolate it.
- **Concurrency:** queries are `&self` (interior-mutable memo), writes are the only `&mut`; reads never
  observe half-applied state. `rayon` for parallel indexing/search; `tokio` for the MCP server.
- **Async only at the edges** (the server/transport). The core is sync and pure.
- **Comments explain *why*** (the design constraint), not what.

---

## 9. Security checklist (verify on every server-touching change)

- [ ] No generic command execution path; every tool parameter is typed and validated.
- [ ] HTTP binds `127.0.0.1` only; bearer token required; never `0.0.0.0`.
- [ ] `code_apply` is gated and sandboxed (escape/`mcp.json`/`.git/hooks` rejected) — covered by tests.
      *(The lexical sandbox is pure/zero-dep; the apply path ALSO resolves symlinks before writing — a symlinked dir can't redirect a write outside root. SCRY-067.)*
- [ ] All returned code carries `source=UNTRUSTED`, and envelope delimiters (`⟦`/`⟧`)
      embedded in returned CONTENT are neutralized so a file can't inject a fake span
      boundary (envelope injection — SCRY-088).
- [ ] Tool descriptions are minimal, honest, stable (no hidden text, no "always use me" bait).
- [ ] Every call and every `apply` is audit-logged (file, diff, time); audit entries
      neutralize control chars (`\n`/`\r`/`\t`) so a crafted summary can't forge or
      corrupt a log line (audit integrity — SCRY-094).
- [ ] No reads outside declared project roots. Dependencies pinned.

---

## 10. Decisions already made — do not re-litigate

- **Agentic multi-step search beats one-shot retrieval** (lexical *or* embedding); the limiter is
  line-level recall. So: support iterative querying and optimize recall.
- **No single modality wins.** Hybrid lexical+structural+graph **with a small reranker** is the design.
  Embeddings are an opt-in *discovery* mode, never the core bet.
- **Anthropic found agentic grep beat their RAG.** Vyer's edge is latency (one round-trip + warm core),
  structure/graph precision, the apply loop, freshness, and tiny footprint — not semantic search alone.
- **Editing, not localization, is the primary bottleneck.** Deterministic apply first.
- **tree-sitter is syntactic only** — it cannot resolve cross-file references; that's the graph layer's job.
- **Code-execution-with-MCP** (tools as on-disk code modules) is the footprint endgame (~98.7% tool-token
  reduction); design the API to be code-drivable.

---

## 11. Definition of done (per increment)

1. `cargo test` green (incl. all freshness + security invariants); `cargo clippy -D warnings` clean.
2. New behavior has tests; new public API is documented.
3. No Rule (§1) violated; SLOs (§7) not regressed for the touched path.
4. Security checklist (§9) passes for server-touching changes.
5. If you changed the interface (§5), update this file and `docs/design-v4.md`.

---

## 12. Kickoff prompt (paste into the first session)

> Read `CLAUDE.md` end to end, then `docs/design-v5.md` and `docs/design-v4.md`. Confirm the workspace
> builds and all tests pass (`cargo test`). Then do **Phase 1** only: rename the `ccx-*` crates to
> `vyer-*`, then flesh out `vyer-server` into a real MCP server over `rmcp` exposing the single `code`
> tool (lexical search via the `grep`/`ignore` crates, **no shelling out**), wired to `vyer-core` for
> compact edge-ordered UNTRUSTED output and to `vyer-incr` for freshness. Enforce the full security
> checklist (§9): stdio by default, localhost+token for HTTP, typed params, audit log, sandboxed gated
> `code_apply`. Add an integration test that starts the server, runs `code` against a fixture repo, and
> asserts a well-formed envelope, plus a red-team test for path-escape and command-execution refusal.
> Do **not** start Phase 2. Keep every existing test green. Show me the diff and the new test output before
> moving on.

---

*Companion docs: `docs/design-v4.md` (full design, evidence, landscape, routing, security, evaluation,
positioning) and `docs/design-v5.md` (the working incremental core). Reference figures are directional —
re-benchmark on your own workload. Re-survey the fast-moving landscape before each major bet.*
