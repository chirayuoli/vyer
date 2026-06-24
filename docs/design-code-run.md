# Design: `code_run` — operator-allowlisted execution + structured diagnostics

> Status: **proposal** (not implemented). Closes the agent's inner loop
> (edit → build/test → structured failure → navigate → fix) without breaking
> the non-negotiable rules in `CLAUDE.md`.

## Motivation

Three independent agent battle-tests converged: vyer owns *read* and *edit*, but
the agent must leave the tool to *run* and *verify*. The back-half — turning
compiler/test output into structured failures mapped to code — already exists as
`mode=diagnose` (now emitting `file:line SEVERITY in SYMBOL :: message` +
window). The missing front-half is **executing** the build/test/lint command.

## The rule tension (and why this resolves it)

- **Rule §3 — no arbitrary command execution.** The forbidden thing is an
  *agent-supplied* command string (`{command, args}` passthrough = the STDIO-RCE
  vector). It is **not** "no execution ever": `EngineConfig.verify_cmd`
  (`--verify "cargo check"`, set at launch, run after each write batch) already
  executes an **operator-configured** command and is rule-compliant. `code_run`
  generalizes exactly that precedent: the agent *selects from* a fixed allowlist,
  it never *supplies* the command.
- **Rule §1 — one tool (+ `code_apply`).** A third top-level tool needs a
  removal/justification. Two compliant shapes (pick one):
  - **(Preferred) Fold into `code_apply`.** Add a `verify: bool` (or
    `tasks: ["test"]`) field: after a successful write batch, run the allowlisted
    task(s) and append the **diagnostics delta** to the apply report. No new tool;
    execution stays welded to the edit it validates — which is exactly where it's
    most valuable.
  - **A gated `code_run` tool**, justified as a distinct *effect class*
    (execute ≠ read ≠ mutate) the harness gates separately. Only viable if §1 is
    amended to "one read tool + one write tool + one execute tool."

## Surface (typed, allowlisted — never a passthrough)

```jsonc
// config (operator, at launch — NOT from any request):
//   --run test="cargo test -q" --run lint="cargo clippy" --run build="cargo build"
// or auto-derived from vyer://project (already detects stack + commands).

// request (agent SELECTS a key; cannot supply a command):
{ "task": "test", "scope": ["crates/vyer-server/**"] }   // scope optional
```

- `task` is validated against the allowlist; an unknown key returns the list of
  configured tasks (actionable error). No `command`/`args`/shell field exists.
- Output is **never raw stderr**: it is piped through `parse_diagnostics` →
  `[{file, line, enclosing_symbol, severity, message, code_excerpt}]`, the same
  structured spans `mode=diagnose` returns today.

## Diagnostics delta (the highest-value piece)

After an edit (or a `code_run`), return only what *changed*:

1. Snapshot diagnostics before the edit (cached by repo revision).
2. Run the allowlisted verify task after the write.
3. Diff the two diagnostic sets → `introduced` / `fixed` / `pre-existing`.
4. Report: *"`_slotRow` edit clean; `foo.dart` added 2 errors at L40, L52."*

Syntax-level deltas are free today (re-parse gate). **Semantic** deltas
(type errors, unresolved refs) require a language-server sidecar — that is
Phase 5/6 work (LSP multiplexer), degrade-and-report when absent.

## Security checklist (must all hold)

- [ ] No `command`/`args`/shell field on any request; `task` is an enum keyed to
      an operator allowlist. Unknown task → list, never execute.
- [ ] Allowlist is set at launch or derived from manifests — never from a request.
- [ ] Execution is off unless explicitly enabled (mirror `--allow-writes`; e.g.
      `--allow-run`), so the default surface has zero execution.
- [ ] Run in the repo root, no shell interpolation of agent input into the argv.
- [ ] Timeout + output cap (DoS); structured output only, audit-logged like apply.
- [ ] Effect class surfaced (`execute`) so the harness can gate it distinctly.

## Phasing

1. **Now (done):** structured `mode=diagnose` output (`file:line sev in sym :: msg`).
2. **Next, cheap:** `code_apply { verify: true }` → run the existing `verify_cmd`
   and return the **syntax** diagnostics delta. (Reuses `verify_cmd` + `parse_diagnostics`.)
3. **Then:** named-task allowlist (`--run k="cmd"`) + auto-derivation from
   `vyer://project`; request `{task}` selection.
4. **Later (Phase 5/6):** LSP sidecar for the semantic diagnostics delta and
   edit-preflight ("this rename leaves 3 unresolved refs — abort?").

## Open decisions (yours)

- Fold into `code_apply` (keeps §1) vs. a third `code_run` tool (needs §1 amendment)?
- Auto-derive tasks from `vyer://project`, require explicit `--run` allowlisting, or both?
- Gate behind a new `--allow-run` flag (recommended) vs. reuse `--allow-writes`?
