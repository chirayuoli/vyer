<div align="center">

# Vyer

**The warm code-context engine for AI coding agents.**

[![npm](https://img.shields.io/npm/v/@0x1labs/vyer?color=cb3837&logo=npm&logoColor=white)](https://www.npmjs.com/package/@0x1labs/vyer)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/chirayuoli/vyer)
[![GitHub](https://img.shields.io/badge/source-chirayuoli%2Fvyer-181717?logo=github)](https://github.com/chirayuoli/vyer)

</div>

---

One MCP tool that gives your coding agent fast, structure-aware sight into a codebase — and a safe,
precise way to change it. Warm, resident, always fresh. **Fully local.** This package downloads the
matching prebuilt `vyer` binary for your platform on install.

## Use it

Vyer is an **MCP server** — your agent host launches it and talks to it over stdio. You don't run
`vyer serve` by hand; you point your host (Claude Code / Cursor / Windsurf / Cline) at it:

```jsonc
// .mcp.json (or your host's MCP config)
{
  "mcpServers": {
    "vyer": { "command": "npx", "args": ["-y", "@0x1labs/vyer", "serve", "--root", ".", "--watch", "--allow-writes"] }
  }
}
```

Claude Code one-liner:

```sh
claude mcp add vyer -- npx -y @0x1labs/vyer serve --root . --watch --allow-writes
```

`--allow-writes` lets Vyer edit (drop it for read-only). Then tell your agent to prefer it:

```sh
npx -y @0x1labs/vyer init      # drops a managed note into ./CLAUDE.md (idempotent)
```

Want a global `vyer` command? `npm install -g @0x1labs/vyer`. To check it from a terminal (no agent),
use `vyer version` or `vyer query "<search>"` — `serve` only does something when an agent drives it.

## What you get

| | |
|---|---|
| **Fast** | a resident warm core — roughly 4 ms a query, never a cold rescan |
| **Precise** | hybrid lexical + structural + graph search; real AST spans, not text blobs |
| **Safe** | deterministic AST edits — atomic, re-parse-checked, never stale after a write |
| **Tiny** | one MCP tool (plus a gated editor) — about 2k tokens of metadata per turn |
| **Polyglot** | 14 languages via tree-sitter (Rust, Python, JS/TS/TSX, Go, C/C++, and more) |
| **Local** | your code never leaves the machine; sandboxed writes; no shelling out |

## One loop, not just search

```jsonc
// find — describe it or name it
{ "queries": [{ "q": "where do we validate the auth token" }] }

// understand — definition + callers + callees + tests, in one call
{ "queries": [{ "q": "validateToken", "detail": "context" }] }

// change — a surgical, atomic edit (this is code_apply)
{ "edits": [{ "locator": "src/auth/token.rs#validate_token", "rename": "verify_token" }] }
```

## Full documentation

Quickstart, the agent playbook, security model, and benchmarks all live in the repo:

### → **https://github.com/chirayuoli/vyer**

No prebuilt binary for your platform? Build from source (needs Rust 1.85+):
`cargo install --git https://github.com/chirayuoli/vyer vyer-server`

<div align="center"><sub>MIT OR Apache-2.0 · built by <a href="https://www.npmjs.com/org/0x1labs">0x1labs</a></sub></div>
