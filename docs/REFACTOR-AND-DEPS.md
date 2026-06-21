# Refactor ops (#3) & dependency-source navigation (#4)

Companion to `docs/AGENT-SUPERPOWERS.md`. Covers what shipped, and the honest design for what's next.

---

## #3 — Refactor ops

### The split that matters: resolution vs. mutation

A refactor is two acts: **resolve** (what is this symbol, where does it live, who references it) and
**mutate** (rewrite the code). Vyer is excellent at resolution — it has a live symbol index — but
*mutation without semantic verification is unsafe*: the re-parse gate proves a result is **syntactically**
valid, not **semantically** correct. An auto-edit can parse fine and still bind the wrong symbol, drop a
capture, or import a path that resolves to nothing.

So the refactors divide cleanly:

| Refactor | Resolve (vyer has this) | Mutate safely | Status |
|---|---|---|---|
| **add-import** | ✅ symbol → defining file (index) | relative imports are mechanical + verifiable | **shipped (resolver)** |
| **rename** | ✅ symbol-aware | already whole-word + re-parse-gated | **already shipped** |
| **move symbol** | ✅ | already re-parse-gated | **already shipped** |
| **change-signature** | ✅ call sites (refs) | needs arg↔param mapping (semantic) | LSP-gated |
| **extract function** | ⚠️ partial | needs free-variable / dataflow analysis | LSP-gated |
| **inline** | ✅ usages | needs capture/aliasing analysis | LSP-gated |

### What shipped now: `detail=import` (SCRY-118)

The reliable half of add-import, as a **read-side** op (zero apply-path risk):

```jsonc
{ "q": "User", "detail": "import", "path": "lib/screens/home.dart" }
//  → import '../models/user.dart';
```

- Resolves `q` to its defining file via the index (honest "not defined / lives in a dependency" when it
  can't — never a wrong guess).
- With `path` (the file to import *into*), builds the exact statement for that file's language:
  **TS/JS/Dart relative imports are exact** (vyer verified the symbol is defined there *and* computes the
  path mechanically); **Python/Rust module paths are best-effort, labelled** so the agent verifies; **Go**
  is package-level → a note. Multi-definition is flagged.
- The agent inserts the returned line with a normal `code_apply` (`@after` the last import / `@new`).

Why read-side and not an auto-inserting `code_apply` op: at the tail of resolution the only thing left is a
one-line insert the agent does trivially — and keeping it read-only means it can **never** corrupt a file
or fight the freshness/atomicity invariants. When the LSP layer lands (and "does this import actually
resolve?" becomes checkable), promoting it to a one-shot `add_import` apply op is a small step.

### When LSP lands, the rest follow

Each LSP-gated refactor becomes safe once we have real binding/type info:
- **change-signature**: resolve every call site (we list them today), map old args → new params *by type*,
  rewrite each, re-parse all, commit all-or-nothing.
- **extract function**: compute the free variables crossing the selection boundary (the params) and the
  written-after-used set (the returns) from the dataflow graph, not guesswork.
- **inline**: substitute the body at each usage only when capture/aliasing analysis proves it's sound.

The sequencing is therefore **LSP (#6) → the auto-mutating refactors**, not before. Shipping them on
syntax alone would violate the project's first rule: never hand back code the agent can't trust.

---

## #4 — Dependency-source navigation

### The problem it solves

The Flutter session's sharpest pain wasn't editing — it was that the agent had to recall the
`flutter_contacts` API *from memory*. Vyer indexes **your repo**, but a coding agent spends half its time
against **library** code it doesn't have memorized. Today, when a symbol isn't in the repo, vyer correctly
says "lives in a dependency" — and then the agent is on its own.

### The design

Dependencies are already on disk, in per-ecosystem caches. Vyer's entire pipeline (walk → parse → symbol
index → search) is source-agnostic — point it at those caches and the agent can **search, read, and
outline a library's real API** exactly like its own code:

| Ecosystem | Where the source lives |
|---|---|
| Dart/Flutter | `~/.pub-cache/hosted/…/<pkg>-<ver>/lib` |
| Rust | `~/.cargo/registry/src/…/<crate>-<ver>` |
| Node | `./node_modules/<pkg>` (often with `.d.ts` types) |
| Python | the active venv's `site-packages/<pkg>` |
| Go | `$GOPATH/pkg/mod/<module>@<ver>` |

**Surface** (no new tool — a scope flag on `code`):
```jsonc
{ "q": "getContacts", "deps": true }              // search the project AND its deps
{ "q": "FlutterContacts", "detail": "outline", "deps": "flutter_contacts" }   // one package's API
```
And `detail=import` (above) gets dramatically stronger: it could then resolve a symbol *into a dependency*
and emit `import 'package:flutter_contacts/flutter_contacts.dart';`.

### The honest tradeoffs (why it's opt-in)

- **Index size / cold start.** `node_modules` can be hundreds of MB. Mitigation: **lazy + on-demand** —
  only index a dependency when a `deps` query references it (resolved via the lockfile/manifest), cache by
  content hash, never index everything eagerly. Default **off**.
- **Which version.** Resolve the *actually-installed* version from the lockfile (`pubspec.lock`,
  `Cargo.lock`, `package-lock.json`), not "latest" — the agent must see the API it's compiling against.
- **Read-only, always.** Dependency sources are **never writable** — the apply sandbox already confines
  writes to the project root; dep paths are outside it and stay that way. Returned dep code is still
  `source=UNTRUSTED`.
- **Types vs. impl.** For TS, prefer the `.d.ts` (the API surface) over compiled JS. For others, the source
  *is* the API.

### Why it fits Vyer

It reuses the existing index/parse/search machinery wholesale — it's a **scope widening**, not a new
subsystem — stays local (the caches are already on the machine), keeps the two-tool surface (a `deps`
flag), and turns the one honest gap the agent hit ("I don't know this library's API") into "navigate it
like your own code." High value, moderate effort, and it makes both `detail=import` and the diagnostics
bridge (dependency stack frames) noticeably better.

### Sequencing

`deps` scope is independent of LSP and reuses everything already built, so it can land **before** the
heavier semantic work — a strong next build after the current superpowers are validated in testing.
