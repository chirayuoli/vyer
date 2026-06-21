# Vyer — new capabilities (try these after reconnecting the MCP)

Everything below was added/fixed this session and is live in `target/release/vyer`.
**Reconnect the `vyer` MCP** (restart the daemon) so it loads the new binary, then try these.
All examples are `code` / `code_apply` tool calls (the only two tools).

## Search & recall

```jsonc
// Substring/prefix recall — used to return nothing (SCRY-038, HIGH bug, fixed)
{ "queries": [{ "q": "valid", "mode": "lexical", "detail": "locate" }] }

// Natural-language query — auto now escalates to semantic when lexical/structural miss (SCRY-039)
{ "queries": [{ "q": "check whether the auth token is valid", "detail": "snippet" }] }

// Ask a natural question that NAMES a function in prose — auto surfaces it (SCRY-057).
// Works for snake_case names mid-sentence; generic words (files/text/path) are ignored.
{ "queries": [{ "q": "how does validate_token decide what to reject", "detail": "snippet" }] }

// Exclude paths with `!` globs (SCRY-040)
{ "queries": [{ "q": "validate_token", "path_scope": ["src/**", "!**/tests/**"] }] }

// Multiple languages in one filter (SCRY-042)
{ "queries": [{ "q": "render", "lang": "ts,js" }] }

// Boolean count — count, not just search, with all_of/any_of/none_of (SCRY-044)
{ "queries": [{ "detail": "count", "all_of": ["unwrap", "expect"] }] }

// Smart-case lexical (now documented): lowercase q is case-insensitive, an
// uppercase letter forces exact case. `engine` finds `Engine`; `Engine` doesn't find `engine`.
{ "queries": [{ "q": "engine", "mode": "lexical", "detail": "count" }] }

// Batched queries now report which matched (SCRY-024) — look for the `per-query found:` note
{ "queries": [{ "q": "login" }, { "q": "logout" }] }
```

## Navigate & understand

```jsonc
// Subtree symbol map — orient on a directory in one call (SP-11)
{ "queries": [{ "detail": "outline", "path_scope": ["src/auth/**"] }] }

// Precise call graph — [calls]/[called by] no longer include comment/param noise (SCRY-043/045)
{ "queries": [{ "q": "validate_token", "detail": "context" }] }
{ "queries": [{ "q": "validate_token", "detail": "impact" }] }   // blast radius

// Every edit you made this session, as a diff — no git needed (SP-10)
{ "queries": [{ "detail": "diff" }] }
```

## Author structural (AST) queries

```jsonc
// 1) Dump a file's tree-sitter node-kinds + field labels (SP-13).
//    Scope to one construct with q=<symbol> (preferred) or lines=<range>.
{ "queries": [{ "path": "src/auth/token.rs", "q": "validate_token", "detail": "ast" }] }

// 2) ...then write a mode=ast query against the kinds you saw
{ "queries": [{ "q": "(function_item name: (identifier) @fn)", "mode": "ast", "lang": "rust" }] }
```

## Edit (all re-parse-validated, gated by --allow-writes)

```jsonc
// Add a member INSIDE a class/impl/struct — any tier-1 language (SP-12).
// Optional @Lstart-end disambiguates a struct from its impl.
{ "edits": [{ "locator": "src/config.rs#@into:Config@L10-40",
              "new_body": "    pub timeout_ms: u64," }] }

// Safe LOCAL-variable rename (SCRY-046): rename every whole-word occurrence of a
// token within ONE symbol's body (repo-wide `rename` would clobber other scopes).
{ "edits": [{ "locator": "src/util.rs#parse", "anchor": "tmp", "replace": "buf", "word": true }] }

// (still available) replace / anchor / rename / move_to / @after / @before / @end / @new / @delete / undo:N
```

## Discover what vyer can do

- Read the `vyer://status` resource — it now advertises `code.modes`, `code.detail`, `code.filters`, `apply.ops`.
- The `code` / `code_apply` tool descriptions are now a when-to-use decision guide.

## Failure help

- A no-match now gives a hint tailored to your query (multi-word → semantic/ast, restrictive scope, lang filter) — SCRY-041.
- A failed `@into`/anchor edit explains the likely cause (e.g. whitespace mismatch, single-line block) instead of a bare "not found".

---
Full rationale + tests: `update.md` (issues SCRY-038→045, superpowers SP-10→13b).
Every item above is covered by unit + integration tests, and apply/diff/ast/security are verified over the real MCP stdio protocol.
