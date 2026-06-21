# Vyer — the coding agent's superpower surface

A plan for what a coding agent *actually* needs (beyond read/write/edit) and which of those Vyer should
own — without losing what makes it sharp.

## The thesis

A coding agent has **senses** (understand the code), **hands** (change the code), and **muscles** (run
things). Today's hosts (Claude Code, Cursor) give it generic muscles (a shell) and weak, generic senses
and hands (grep, whole-file reads, whole-file rewrites).

**Vyer's job is to make the senses superhuman and the hands surgical** — and to stay out of the muscles'
way. The superpower isn't any one feature; it's the **tight loop**:

> Vyer pinpoints exactly what/where → the agent edits precisely → the host runs it → **Vyer maps the
> errors back to code** → repeat. Fast, structured, fresh, local.

Two hard constraints keep Vyer must-have instead of bloated, and every idea below respects them:
- **Two tools only** (`code` + gated `code_apply`). New powers ship as a `detail`/`mode`/op value or a
  read-only **Resource** — never a third tool. (Tool-selection accuracy collapses past ~30–50 tools.)
- **No arbitrary execution.** The host runs build/test/scaffold/git. Vyer never shells out. This is a
  security boundary *and* a focus boundary: Vyer is everything-about-the-code, not a task runner.

## What a coding agent actually does (the real loop)

| Phase | What it needs | Vyer today |
|---|---|---|
| **Orient** | "What is this repo? structure, entry points, conventions" | repo-map (PageRank), outline, status ✅ |
| **Find** | locate code by name / concept / pattern | hybrid lexical+structural+graph+semantic+ast ✅ |
| **Read** | files, ranges, the enclosing symbol | read-by-path + lines, AST-expanded snippets ✅ |
| **Understand** | callers, callees, blast radius, **types**, deps | refs/context/impact — but `graph=approx`, no types ⚠️ |
| **Edit** | replace, rename, move, insert, delete, create, **refactor** | full apply ops; rename/move but no extract/inline/sig-change ⚠️ |
| **Verify** | does it compile / pass tests / lint? | `--verify` runs a cmd post-write, reports pass/fail ⚠️ |
| **Diagnose** | map a compiler/test error → the exact code | **gap** — agent re-greps the error text by hand ❌ |
| **Iterate** | edit → verify → diagnose, tightly | partial (no error→location bridge) ⚠️ |
| **Track** | what have I changed; undo | `detail=diff`, `undo` ✅ |
| **Run** | build, test, scaffold, package managers | **host's job** (by design) ➖ |
| **Investigate history** | blame, recent changes, *why* | **gap** — no git awareness ❌ |
| **Use dependencies** | "what's the API of this library?" | **gap** — only the project is indexed, not its deps ❌ |

The ✅s are why Vyer already feels good for "Dart core" work. The ❌/⚠️s are where the agent fell back to
native tools, guessed, or did manual grep-the-error work.

## The plan — five superpowers that fit cleanly, then the big bets

### Tier 1 — high leverage, fits the architecture now

**1. Diagnostics bridge (error → location).** *The single highest-value gap.*
The agent runs `flutter analyze` / `cargo test` / `tsc` (via the host) and gets a wall of errors. Today it
re-greps each one by hand. Vyer should take a compiler/test/stack-trace blob and return the precise
locators + surrounding context — `mode=diagnose` (or `detail=diagnose`) that parses the common formats
(rustc, tsc, dart, pyflakes/pytest, jest, go) into `file#symbol@Lline` spans, best-at-the-edges.
And: enrich **`--verify`** so that after a write batch it doesn't just say "failed" — it returns *where*
it broke as locators. This closes the edit→verify→**diagnose**→fix loop, which is most of debugging.

**2. Git-awareness (local, no shell).** Read `.git` directly via a Rust git lib (gitoxide) — never shelling out.
- `detail=blame` — who/when last changed each line of a span (+ the commit subject = *why*).
- `detail=history` — recent commits touching a symbol/file.
- a scope filter **`changed:<ref>`** — restrict search to code changed since a branch/commit ("review my
  diff", "what's new since main"). Gives the agent temporal + authorship context it currently can't see.

**3. Dependency-source navigation.** *The Flutter session's real pain.* The agent had to know the
`flutter_contacts` API from memory. Vyer should optionally index the **dependency sources** (the local
package cache: `~/.pub-cache`, `~/.cargo/registry`, `node_modules`, site-packages) so `code` can search/
read/outline a library's API the same way it does the project. `lang`/`scope` already exist; add a
`deps:true` scope. Turns "guess the library API" into "navigate it."

**4. Project intelligence (a Resource, not a tool).** A new read-only resource `vyer://project` that
surfaces: detected project type/framework, **the actual build/test/run/lint commands** (parsed from
`package.json` scripts, `Cargo.toml`, `pubspec.yaml`, `Makefile`, `justfile`…), entry points, and test
layout. So the agent *knows* "to test, run `flutter test`" instead of guessing — and hands that to the
host to execute. This is the clean bridge between Vyer (knows the code) and the host (runs it).

**5. Refactor ops in `code_apply`.** Beyond rename/move:
- **change-signature** — edit a function's params and update every call site (uses the ref graph).
- **extract function / extract variable / inline** — AST-mechanical, deterministic, re-parse-validated.
- **add-import** — when an edit references an unimported symbol, add the import (best-effort by symbol→
  module mapping; precise once #6 lands). These are the edits agents do constantly and get wrong by hand.

### Tier 2 — the big bets (already on the roadmap; this is where "superhuman senses" really land)

**6. LSP-grade navigation.** A best-effort LSP sidecar for tier-1 languages turns `graph=approx` into
exact: real cross-file go-to-definition, find-implementations, **type-of/hover**, signature help, and
true diagnostics. This is the difference between "probably the caller" and "the caller, type-checked."
Degrade to today's approximation when no server exists.

**7. Neural semantic search (opt-in).** Contextual embeddings + reranker for the "I don't know the name,
find the code that does X" case. Vyer has lexical-subword today; neural is the lift for concept search.
Strictly opt-in, clearly labeled, never masquerading as exact.

### Tier 3 — ergonomics (small, nice)

**8. Session summary + TODO scan.** `detail=diff` already shows session edits; add a one-call session
summary ("files touched, symbols changed, net diff") and a `detail` that surfaces `TODO/FIXME/HACK`
markers so the agent can pick up loose ends.

## Sequencing

1. **Diagnostics bridge + `--verify` locators** (1) — biggest loop-closer, pure logic, no new deps.
2. **Git-awareness** (2) — high value, one local dep (gitoxide), no shell.
3. **Project intelligence resource** (4) — cheap, unlocks correct host execution.
4. **Dependency-source navigation** (3) — reuses the whole index pipeline, just widens the scope.
5. **Refactor ops** (5) — incremental, each is a deterministic AST transform + re-parse gate.
6. **LSP** (6) then **neural semantic** (7) — the heavy, high-ceiling bets.

## Why this is still Vyer, not bloat

Every item above lands as a `detail`/`mode`/op on the existing two tools or a read-only Resource — the
surface stays at **two tools**. None of it shells out — the host keeps the muscles. All of it deepens the
same thesis: **make the agent's understanding of, and edits to, a codebase superhuman — fast, structured,
fresh, and local.** The agent stops falling back to grep, stops guessing library APIs, stops hand-mapping
errors, and stops rewriting whole files — which is exactly the "feels like it was built for this
generation" outcome.
