<div align="center">

# Vyer

*Vyer · rhymes with "buyer" · from the Swedish for "views, vistas, visions"*

**The warm code-context engine for AI coding agents.**

Give your coding agent fast, structure-aware sight into a codebase — and a safe, precise way to change
it. Everything stays warm and resident, so the moment you write, the next read already knows. All on
your machine.

[Quickstart](#quickstart) · [Why Vyer](#why-vyer) · [How it works](#how-it-works) · [Security](#security--trust) · [Status](#status)

MIT OR Apache-2.0 · local-first · 14 languages · ~4 ms warm queries · 217 tests

</div>

---

## The problem

Coding agents are brilliant in the abstract and clumsy in your repo. Watch one work and you'll see it:

- **Read whole files just to find one function** — pouring tokens into context and quietly getting dumber
  as the window fills (the lost-in-the-middle problem is real).
- **Re-read the same files every single turn**, because nothing it learned a moment ago stuck around.
- **Confuse a definition with a comment that mentions it** — there's no sense of structure, just text.
- **Edit by rewriting the entire file** with the big model. Slow, pricey, and one fat-fingered line away
  from breaking the build.
- **Trip over its own changes** — edit a file, read it back, and get the *old* version.

It's a smart developer using last decade's tools. Vyer hands it this decade's.

## What Vyer does

Vyer is a small, always-on engine that lives next to your code and talks to your agent through a single
MCP tool. It blends three kinds of search (lexical, structural, and graph), hands back tight,
best-bits-first snippets, and — when it's time to change something — does the edit precisely and safely.

The trick is that it stays *warm*. Your code is parsed, indexed, and graphed in memory, so a query is a
few milliseconds, not a cold rescan. And the instant the agent writes a change, the core updates itself —
so the very next read is correct. No stale answers, ever.

```
            +---------------------- AI agent -----------------------+
            |   code        (search, read, navigate)                |
            |   code_apply  (rename, move, insert, delete, ...)     |
            +---------------------------+---------------------------+
                                        | one MCP round-trip
            +---------------------------v---------------------------+
            |  Vyer - incremental warm core (resident, local)       |
            |  hybrid search, AST spans, repo-map, reference graph  |
            |  deterministic apply, read-after-write staleness = 0  |
            +-------------------------------------------------------+
```

## Why Vyer

Plenty of MCP servers wrap `grep` and call it a day. Vyer is the whole loop — find, understand, *and*
change — done fast, safe, and local. Here's the difference in one table:

| | Native tools / grep-wrapper MCPs | Vyer |
|---|---|---|
| Speed | cold start, re-scan, every call | stays warm — about 4 ms a query |
| Freshness | might hand back stale code | the read after a write is always correct (a hard rule) |
| Precision | it's all just text | knows defs from comments from strings; returns real AST spans |
| Editing | the big model rewrites the file | a surgical AST edit, checked by re-parsing, all-or-nothing |
| Context cost | dumps whole files at you | tight, budgeted, best-bits-first snippets |
| Footprint | dozens of tools, billed every turn | one tool (plus a gated editor) — tiny |
| Trust | uploads your code somewhere | none of it leaves your machine |

Even Anthropic found that agentic `grep` beat their fancy RAG setup. So Vyer doesn't try to out-embed
anyone — it wins on the things that actually matter in a repo: it's *fast* (one round-trip, already warm),
it *understands structure*, it can *safely make the change*, it's *never stale*, and it barely costs the
model any attention to use.

## Quickstart

Three steps, about a minute.

**1. Install it.** No toolchain, no clone:

```sh
npx -y @0x1labs/vyer serve --root .                # via npm — just needs Node
brew install chirayuoli/tap/vyer               # via Homebrew
# …or download a prebuilt binary from the Releases page (macOS / Linux / Windows)
```

Prefer to build from source? No clone needed either (needs the Rust toolchain):

```sh
cargo install --git https://github.com/chirayuoli/vyer vyer-server   # Cargo fetches + builds it
```

> The npm / Homebrew / prebuilt channels light up with the first tagged release (`v0.1.0`) —
> the pipeline is wired (`docs/RELEASING.md`). Building from source works today.

**2. Point your agent at it.** Vyer indexes the current repo automatically and talks over stdio, so
there's no network and nothing to configure:

```jsonc
// Claude Code / Cursor / Windsurf  ->  .mcp.json (or your host's MCP config)
{
  "mcpServers": {
    "vyer": { "command": "vyer", "args": ["serve", "--root", ".", "--watch"] }
  }
}
```

Want it to edit too? Add `--allow-writes`.
On Claude Code you can skip the JSON: `claude mcp add vyer -- vyer serve --root . --watch`.

**3. Tell your agent to actually use it.** One command drops a short, managed note into your `CLAUDE.md`:

```sh
vyer init            # -> ./CLAUDE.md   (safe to re-run; never touches your own notes)
vyer init --global   # -> ~/.claude/CLAUDE.md   (every project at once)
```

Done. Your agent now searches, reads, and edits through Vyer — and does it in batches, at warm-core speed.

## Feature tour

**Find things** — `code`

```jsonc
{ "queries": [{ "q": "validateToken", "detail": "snippet" }] }           // know the name? grab it
{ "queries": [{ "q": "where do we check auth token expiry" }] }          // don't? describe it in words
{ "queries": [{ "detail": "outline", "path_scope": ["src/auth/**"] }] }  // get the lay of the land
```

Search how you like: `mode` is auto, lexical, structural, graph, semantic, or ast. Ask for as much or as
little as you need with `detail`: locate, outline, snippet, full, refs, impact, context, count (that's
`grep -c`), tree (that's `ls`/`find`), diff, ast. Narrow it with `lang`, `path_scope` globs, or boolean
`all_of`/`any_of`/`none_of`. You can read plain files too — `path` plus `lines: "40-80"` replaces
Read/sed/head/tail.

**Understand things** — the whole story in one call

```jsonc
{ "queries": [{ "q": "validateToken", "detail": "context" }] }   // its definition + who calls it + what it calls + its tests
{ "queries": [{ "q": "validateToken", "detail": "impact" }] }    // what breaks if I touch this?
```

**Change things** — `code_apply` (gated, atomic, re-parsed before it's trusted, never stale after)

```jsonc
{ "edits": [{ "locator": "src/auth/token.rs#validate_token", "rename": "verify_token" }] }
{ "edits": [{ "locator": "src/auth/token.rs#validate_token", "new_body": "..." }], "dry_run": true }
```

Rename across the whole repo (or just one package in a monorepo), replace a body, move a symbol to another
file, insert with `@after`/`@before`/`@into`/`@end`/`@new`, delete, or `undo`. A batch of edits lands all
at once or not at all — no half-finished refactors.

## How it works

- **It stays warm.** File contents go in; parses, symbols, outlines, the search index, the repo-map
  (PageRank), and the reference graph come out — all memoized. Change one file and only that file's chain
  recomputes, in under 50 ms. The next query is already fresh.
- **It doesn't bet on one kind of search.** Cheap, exact methods run first and get fused with
  reciprocal-rank fusion; it only reaches for semantic when it's genuinely unsure — so a real match never
  loses to a fuzzy guess.
- **It respects the model's attention.** The best spans go first *and* last (models skim the middle), and
  every span is labeled `source=UNTRUSTED` — returned code is data to reason about, never instructions to
  obey.
- **It edits like a surgeon, not a sledgehammer.** Find the symbol's AST node, splice the change in,
  re-parse to make sure it still compiles structurally, write, and refresh the core — all in one beat. A
  model-based fallback exists, but it's the exception, not the rule.
- **It speaks 14 languages** via tree-sitter: Rust, Python, JavaScript, TypeScript, TSX, Go, Dart, Java,
  Ruby, Swift, Kotlin, C, C++, C#, and PHP — JSX, raw/template/triple-quoted strings, and CRLF/BOM files
  included.

## Security & trust

The honest answer to "should I let an agent index and edit my code?"

- **It's all local.** Your repo never leaves the machine. Nothing is uploaded.
- **No shelling out, ever.** Typed operations only, every parameter checked — there's no generic command
  to hijack.
- **Edits are fenced in.** Writes can't escape the project root; `mcp.json`, `.git/hooks`, and sneaky
  symlinks are all refused.
- **Returned code can't lie to the model.** Every span is `source=UNTRUSTED`, and any envelope markers
  hiding inside a file are neutralized, so a file can't forge a fake result boundary.
- **No open ports by default.** stdio only; the optional HTTP mode binds to `127.0.0.1` and demands a
  bearer token.
- **Everything is logged.** Every call and every edit, with a diff.

## Performance

Warm and resident, on a normal repo:

| Operation | Target (SLO) | Measured |
|---|---|---|
| `locate` / `outline` | p50 < 30 ms | ~4 ms |
| `snippet` (AST-expanded) | p50 < 50 ms | ~4 ms |
| `refs` (graph) | p50 < 150 ms | ~11 ms |
| re-index one edited file | < 50 ms | ~18 ms at 50k files |
| read-after-write staleness | 0 | 0 |

It's been pushed to 50,000 files (still 5–12 ms warm; a cold start takes about 1.3 s). And these targets
aren't aspirational — they're checked in CI, so a slowdown fails the build instead of quietly slipping by.

## Status

Solid and ready to use: 217 workspace tests plus 24 real subprocess smoke tests, clippy clean, SLOs
enforced in CI. Working today: hybrid search, the full apply path, the repo-map and reference graph, MCP
Resources, a filesystem watcher, 14-language parsing, and the security posture above. Coming next: a full
LSP graph for true cross-file resolution, opt-in neural embeddings, and an encrypted, opt-in team index.

```
crates/vyer-core    pure logic, zero deps  - locators, RRF, budgeting, ordering, sandbox, output
crates/vyer-incr    warm incremental core, zero deps  - memoization, freshness, selective recompute
crates/vyer-index   tree-sitter parsing (14 languages)
crates/vyer-server  the secure MCP server and `vyer` CLI
```

Want to drive it like a pro? Read `docs/AGENT-PLAYBOOK.md` (intent to optimal call). The full design and
reasoning live in `CLAUDE.md` and `docs/`.

## License

MIT OR Apache-2.0 — your choice.
