# vyer-server test layers

Vyer is verified at five escalating levels — a regression at any layer fails CI.
Run everything with the one-command gate: **`bash scripts/gate.sh`**
(build → workspace tests → clippy `-D warnings` → `fmt --check` → binary smoke → warm SLO,
failing fast with a clear `FAIL:`/`GATE PASSED` line). Or the pieces:
`cargo test --workspace && cargo clippy --all-targets -- -D warnings`,
plus the binary smoke: `cargo build --release -p vyer-server && bash scripts/smoke.sh`.

| Layer | Where | What it locks |
|---|---|---|
| **Unit** | `src/**` `#[cfg(test)]` (engine, apply, lexical, core, incr, index) | Pure logic: fusion, packing, ordering, locator, sandbox, freshness invariants, `scan_idents`/`postings_substring`/`path_in_scope` edge cases, every `detail`/`mode` branch, every apply op, the CLI `positional()` parser. |
| **Integration** | `tests/integration.rs` | The engine end-to-end against real temp-dir repos: search/read/apply over the shared dispatch, **read-after-write freshness (staleness=0)**, SP-2 atomic multi-edit rollback (incl. `@into`), the sandbox refusing escapes / `mcp.json` / `.git/hooks`, and the localhost HTTP transport (token required, loopback-only, round-trip). |
| **CLI** | `tests/cli_smoke.rs` | The real `vyer` binary's `query`/`apply` subcommands via `CARGO_BIN_EXE_vyer`: each `detail` view, the positional-query parser, and `apply --rename` dry-run + the `--undo` session-only message. |
| **Real MCP protocol** | `tests/stdio_e2e.rs` | The binary as a subprocess speaking JSON-RPC over stdio: `initialize` → `tools/list` → `tools/call` (search, `detail=ast`) → `resources/*`; the **write path** (`code_apply` + `detail=diff`) with `--allow-writes`; and the **red-team** gate — `code_apply` refused without `--allow-writes`, and escape/`mcp.json`/`.git/hooks` refused even *with* it (no file created). |
| **Binary smoke** | `scripts/smoke.sh` | The release binary across every `detail`/`mode`/filter/subcommand vs this repo — a fast manual regression catch (panics + expected output). Prints `N passed, M failed`. |

## Design invariants every layer protects (see `CLAUDE.md` §1)

- **Freshness = 0**: a read right after a write sees the new code (no stale cache).
- **No arbitrary command execution**: typed params only; the only shell-out is the operator-set `--verify` command.
- **Atomic apply**: a batch with one bad edit changes zero files (disk *and* warm core).
- **Sandboxed writes**: confined to the project root; `mcp.json`/`.git/hooks`/escapes refused, gated by `--allow-writes`.
- **Localhost + token HTTP**: never binds non-loopback; every request needs `Authorization: Bearer`.
