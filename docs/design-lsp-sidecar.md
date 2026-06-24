# Design: LSP sidecar — semantic depth (type-resolved nav + diagnostics delta)

> Status: **proposal** (Phase 5/6). The last true capability gap: vyer's graph
> (`refs`/`impact`/`context`/blast-radius/safe-delete) is a lexical+tree-sitter
> *approximation* (`graph=partial(approx)`). True cross-file, type-resolved
> semantics needs a language server. This is a multi-step build — do it in the
> phases below, each independently landable with tests staying green.

## Why (the gap the guardrails can't close)
- `safe-delete` / `blast-radius` / `refs` count a NAME across files — a same-named
  symbol in another scope is a false positive; a method resolved through a type is
  a false negative. Only a type-resolver fixes both.
- **Edit-preflight** — "this rename leaves 3 unresolved references; abort?" — and a
  **semantic diagnostics delta** after an edit ("your edit added 2 type errors at
  L40/L52") require type information vyer does not compute today.

## Non-negotiables it must honor
- **Rule §1 (two tools):** no third tool. Surface via existing seams —
  `detail=refs/impact/context` get *upgraded* (tier reported), and the diagnostics
  delta rides the `code_apply` report (next to the existing re-parse/verify lines).
- **Rule §8 (degrade, don't crash):** the sidecar is best-effort. No server for a
  language, a crash, or a timeout → fall back to the current lexical/tree-sitter
  approximation and report the tier honestly (`graph=full|partial|none`,
  `tier=lsp|lexical-approx`). Never block a query/edit on the sidecar.
- **Rule §9 (security):** language servers are external processes — operator-gated
  (`--allow-lsp`), discovered from a fixed allowlist of known servers, never a
  request-supplied binary. Same trust model as `verify_cmd`/`code_run`.
- **Rule §9 (no reads outside root):** the server runs rooted at the project; its
  results are filtered to in-root paths before they reach the agent.

## Architecture
```
Engine ──► trait SemanticProvider {
              fn references(file, symbol, pos) -> Option<Vec<Ref>>;   // find-refs
              fn definition(file, pos)         -> Option<Loc>;         // go-to-def
              fn diagnostics(files)            -> Option<Vec<Diag>>;   // pull diags
              fn tier()                        -> Tier;                // full|partial|none
           }
   ├─ NullProvider           (default; tier=none → today's approximation is used)
   └─ LspProvider(lang)      (rust-analyzer / tsserver / pyright / gopls / dart …)
        • spawn once, long-lived, JSON-RPC over stdio (lsp-types crate)
        • didOpen/didChange synced from the warm core on set_text (freshness)
        • request timeouts + crash supervisor → degrade to NullProvider for that lang
```
The provider is consulted as an *upgrade*: the existing approximate result is
computed first (so a miss/timeout costs nothing), and the LSP result, when present,
*replaces* it and bumps the reported tier.

## Phases (each lands green)
1. **Seam + honest degradation (no external dep).** Introduce `SemanticProvider`
   with `NullProvider` wired into `refs/impact/context` and the apply report. No
   behavior change (tier=none everywhere) — but the integration points + tier
   reporting + tests exist. *Done when:* tier shows in output, all tests green.
2. **One tier-1 server, read-only (rust-analyzer).** `--allow-lsp`; spawn + LSP
   handshake + `textDocument/references` & `definition`; sync didOpen/didChange from
   the warm core. `detail=refs` reports `tier=lsp` when available. *Done when:*
   find-refs is type-accurate on a Rust fixture; a killed server degrades and the
   test passes.
3. **Diagnostics delta on edit.** After `code_apply`, pull diagnostics for the
   touched files, diff vs the pre-edit snapshot → `introduced/fixed`, appended to the
   report ("`foo.rs` +2 errors at L40,L52"). *Done when:* an edit that breaks types
   reports the new errors; a clean edit reports none.
4. **Edit-preflight.** On `rename`/signature-changing `new_body`, query refs and, if
   the edit would leave unresolved references, refuse (force-overridable) — the
   semantic upgrade of SCRY-138/139. *Done when:* a refs-breaking rename is caught.
5. **Fan out servers:** tsserver/pyright/gopls/dart, each behind the same trait +
   supervisor, each degrading independently.

## Cost / risk
- Heavy: `lsp-types` + async process mgmt + per-language server binaries (not bundled
  — discovered on PATH or operator-pointed). CI can't download servers, so phases 2+
  test the *degradation* path in CI and the *happy* path behind a feature flag / local
  fixture. This is why it's its own multi-session effort, not a single commit.

## Open decisions (operator/maintainer)
- Bundle/download servers vs. require-on-PATH (lean: require-on-PATH + clear status).
- `--allow-lsp` default off (recommended) vs. auto-on when a server is found.
- Memory ceiling: long-lived servers are RAM-heavy; cap concurrent servers (LRU).
