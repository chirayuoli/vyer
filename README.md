<div align="center">

# Vyer

**The warm code-context engine for AI coding agents.**

One MCP tool that gives your agent fast, structure-aware sight into a codebase — and a safe,
precise way to change it. Warm, resident, always fresh. Fully local.

[![CI](https://github.com/chirayuoli/vyer/actions/workflows/ci.yml/badge.svg)](https://github.com/chirayuoli/vyer/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/chirayuoli/vyer?color=2ea043)](https://github.com/chirayuoli/vyer/releases)
[![npm](https://img.shields.io/npm/v/@0x1labs/vyer?color=cb3837&logo=npm&logoColor=white&label=%400x1labs%2Fvyer)](https://www.npmjs.com/package/@0x1labs/vyer)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Built with Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)

[**Quickstart**](#quickstart) · [Why Vyer](#why-vyer) · [How it works](#how-it-works) · [Security](#security--trust) · [Performance](#performance)

</div>

---

<table>
<tr>
<td width="33%" valign="top">

### Fast

A resident, warm core — roughly **4 ms** a query. No cold rescans, no re-reading the same files every
turn.

</td>
<td width="33%" valign="top">

### Precise

Hybrid **lexical + structural + graph** search. It knows a definition from a comment from a string, and
returns real AST spans — not text blobs.

</td>
<td width="33%" valign="top">

### Safe

**Deterministic AST edits**, re-parse-validated and atomic. The read after a write is never stale. And
none of your code leaves the machine.

</td>
</tr>
</table>

---

## The problem

Coding agents are brilliant in the abstract and clumsy in your repo. Watch one work and you'll see it:

- **Read whole files just to find one function** — pouring tokens into context and quietly getting dumber
  as the window fills (the lost-in-the-middle problem is real).
- **Re-read the same files every single turn**, because nothing it learned a moment ago stuck around.
- **Confuse a definition with a comment that mentions it** — there's no sense of structure, just text.
- **Edit by rewriting the entire file** with the big model. Slow, pricey, and one fat-fingered line away
  from breaking the build.
- **Trip over their own changes** — edit a file, read it back, and get the *old* version.

It's a smart developer using last decade's tools. Vyer hands it this decade's.

## What Vyer does

Vyer is a small, always-on engine that lives next to your code and talks to your agent through a single
MCP tool. It blends three kinds of search (lexical, structural, and graph), hands back tight,
best-bits-first snippets, and — when it's time to change something — does the edit precisely and safely.

The trick is that it stays *warm*. Your code is parsed, indexed, and graphed in memory, so a query is a
few milliseconds, not a cold rescan. And the instant the agent writes a change, the core updates itself,
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
change — done fast, safe, and local:

| | Native tools / grep-wrapper MCPs | **Vyer** |
|---|---|---|
| **Speed** | cold start, re-scan, every call | stays warm — about **4 ms** a query |
| **Freshness** | can hand back stale code | the read after a write is **always** correct (a hard rule) |
| **Precision** | it's all just text | knows defs from comments from strings; real AST spans |
| **Editing** | the big model rewrites the file | a surgical AST edit, re-parse-checked, all-or-nothing |
| **Context cost** | dumps whole files at you | tight, budgeted, best-bits-first snippets |
| **Footprint** | dozens of tools, billed every turn | **one** tool (plus a gated editor) — tiny |
| **Trust** | uploads your code somewhere | none of it leaves your machine |

> Even Anthropic found that agentic `grep` beat their fancy RAG setup. So Vyer doesn't try to out-embed
> anyone — it wins on what actually matters in a repo: it's *fast* (one round-trip, already warm), it
> *understands structure*, it can *safely make the change*, it's *never stale*, and it barely costs the
> model any attention to use.

## Quickstart

**1. Install it** — pick one, all live:

| Channel | Command |
|---|---|
| **npm** (just needs Node) | `npm install -g @0x1labs/vyer` |
| **Homebrew** | `brew install chirayuoli/tap/vyer` |
| **Prebuilt binary** | grab it from [Releases](https://github.com/chirayuoli/vyer/releases) (macOS / Linux / Windows) |
| **From source** | `cargo install --git https://github.com/chirayuoli/vyer vyer-server` |

> You don't run `vyer serve` by hand — it's an MCP server your agent host launches (step 2). With the
> npm config below you don't even need to install anything; `npx` fetches it on demand.

**2. Point your agent at it.** Vyer auto-indexes the current repo and talks over stdio — no network,
nothing to configure:

```jsonc
// Claude Code / Cursor / Windsurf  ->  .mcp.json (or your host's MCP config)
{
  "mcpServers": {
    "vyer": { "command": "npx", "args": ["-y", "@0x1labs/vyer", "serve", "--root", ".", "--watch", "--allow-writes"] }
  }
}
```

`--allow-writes` lets Vyer edit (the point is find *and* fix) — drop it for read-only. On Claude Code
you can skip the JSON:
`claude mcp add vyer -- npx -y @0x1labs/vyer serve --root . --watch --allow-writes`.

**3. Tell your agent to actually use it.** One command drops a short, managed note into your `CLAUDE.md`
so the agent reaches for Vyer (and works in batches) instead of native tools:

```sh
vyer init            # -> ./CLAUDE.md   (safe to re-run; never touches your own notes)
vyer init --global   # -> ~/.claude/CLAUDE.md   (every project at once)
```

That's it. Your agent now searches, reads, and edits through Vyer — in batches, at warm-core speed.

## Feature tour

<details open>
<summary><b>Find things</b> — one <code>code</code> call, however you think about it</summary>

```jsonc
{ "queries": [{ "q": "validateToken", "detail": "snippet" }] }           // know the name? grab it
{ "queries": [{ "q": "where do we check auth token expiry" }] }          // don't? describe it in words
{ "queries": [{ "detail": "outline", "path_scope": ["src/auth/**"] }] }  // get the lay of the land
```

`mode`: auto · lexical · structural · graph · semantic · ast.
`detail`: locate · outline · snippet · full · refs · impact · context · count (`grep -c`) · tree
(`ls`/`find`) · diff · ast. Narrow with `lang`, `path_scope` globs, or boolean `all_of`/`any_of`/`none_of`.
Read plain files too — `path` plus `lines: "40-80"` replaces Read/sed/head/tail.

</details>

<details>
<summary><b>Understand things</b> — the whole story in a single call</summary>

```jsonc
{ "queries": [{ "q": "validateToken", "detail": "context" }] }   // definition + callers + callees + tests
{ "queries": [{ "q": "validateToken", "detail": "impact" }] }    // what breaks if I touch this?
```

</details>

<details>
<summary><b>Change things</b> — <code>code_apply</code>, gated, atomic, re-parsed, never stale after</summary>

```jsonc
{ "edits": [{ "locator": "src/auth/token.rs#validate_token", "rename": "verify_token" }] }
{ "edits": [{ "locator": "src/auth/token.rs#validate_token", "new_body": "..." }], "dry_run": true }
```

Rename across the whole repo (or just one package in a monorepo), replace a body, move a symbol to
another file, insert with `@after`/`@before`/`@into`/`@end`/`@new`, delete, or `undo`. A batch lands all
at once or not at all — no half-finished refactors.

</details>

## How it works

<details>
<summary>The four ideas that make it fast, precise, and safe</summary>

<br>

- **It stays warm.** File contents go in; parses, symbols, outlines, the search index, the repo-map
  (PageRank), and the reference graph come out — all memoized. Change one file and only that file's chain
  recomputes, in under 50 ms. The next query is already fresh.
- **It doesn't bet on one kind of search.** Cheap, exact methods run first and fuse via reciprocal-rank
  fusion; it only reaches for semantic when genuinely unsure — so a real match never loses to a fuzzy one.
- **It respects the model's attention.** The best spans go first *and* last (models skim the middle), and
  every span is labeled `source=UNTRUSTED` — returned code is data to reason about, never instructions.
- **It edits like a surgeon, not a sledgehammer.** Find the symbol's AST node, splice the change in,
  re-parse to confirm it's still valid, write, and refresh the core — all in one beat. A model fallback
  exists, but it's the exception.

**14 languages** via tree-sitter: Rust · Python · JavaScript · TypeScript · TSX · Go · Dart · Java ·
Ruby · Swift · Kotlin · C · C++ · C# · PHP — including JSX, raw/template/triple-quoted strings, CRLF/BOM.

</details>

## Security & trust

The honest answer to "should I let an agent index and edit my code?"

- **It's all local.** Your repo never leaves the machine. Nothing is uploaded.
- **No shelling out, ever.** Typed operations only, every parameter checked — no generic command to hijack.
- **Edits are fenced in.** Writes can't escape the project root; `mcp.json`, `.git/hooks`, and sneaky
  symlinks are refused.
- **Returned code can't lie to the model.** Every span is `source=UNTRUSTED`, and envelope markers hidden
  inside a file are neutralized so a file can't forge a fake result boundary.
- **No open ports by default.** stdio only; the optional HTTP mode binds `127.0.0.1` and requires a token.
- **Everything is logged** — every call and every edit, with a diff.

## Performance

Warm and resident, on a normal repo:

| Operation | Target (SLO) | Measured |
|---|---|---|
| `locate` / `outline` | p50 < 30 ms | **~4 ms** |
| `snippet` (AST-expanded) | p50 < 50 ms | ~4 ms |
| `refs` (graph) | p50 < 150 ms | ~11 ms |
| re-index one edited file | < 50 ms | ~18 ms at 50k files |
| read-after-write staleness | 0 | **0** |

Pushed to **50,000 files** (still 5–12 ms warm; cold start ~1.3 s). These targets are **enforced in CI** —
a slowdown fails the build instead of quietly slipping by.

## Project status

Solid and ready to use: **217 workspace tests** plus 24 real subprocess smoke tests, clippy clean, SLOs
enforced in CI. Working today: hybrid search, the full apply path, the repo-map and reference graph, MCP
Resources, a filesystem watcher, 14-language parsing, and the security posture above. On the roadmap: a
full LSP graph for true cross-file resolution, opt-in neural embeddings, and an encrypted team index.

```
crates/vyer-core    pure logic, zero deps  - locators, RRF, budgeting, ordering, sandbox, output
crates/vyer-incr    warm incremental core, zero deps  - memoization, freshness, selective recompute
crates/vyer-index   tree-sitter parsing (14 languages)
crates/vyer-server  the secure MCP server and `vyer` CLI
```

Driving it like a pro: [`docs/AGENT-PLAYBOOK.md`](docs/AGENT-PLAYBOOK.md) (intent → optimal call). The full
design lives in [`CLAUDE.md`](CLAUDE.md) and [`docs/`](docs/); releasing is in
[`docs/RELEASING.md`](docs/RELEASING.md).

## License

**MIT OR Apache-2.0**, at your option.

<div align="center"><sub>Built by <a href="https://www.npmjs.com/org/0x1labs">0x1labs</a> · local-first code context for the agentic engineering age.</sub></div>
