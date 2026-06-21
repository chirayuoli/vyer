# Vyer — pluggable language architecture

> **STATUS: IMPLEMENTED (Phase A + B) + Dart pack shipped.** The generic `tags`-query extractor is
> live in `vyer-index` (`mod langpack`); all 5 original languages now run through it, the hand-written
> Go-receiver / JS-arrow special cases are deleted, and **Dart/Flutter** is a first-class pack. 98 tests
> green, clippy clean. Verified live on a mixed Dart+Python+CSS repo. The sections below are the design;
> §5 records the as-built result and answers the cold-start / isolation / mixed-language questions.

> Goal: make vyer work **perfectly for any language**, where adding one is ~6 lines + a dependency
> (not edits across 7 places in 3 crates), and where a Flutter/Dart project only ever loads the Dart
> flow — never the Python/Rust grammars.

---

## 0. The core realization

Every tree-sitter grammar ships `queries/tags.scm` — the *maintainer's own* declarative spec for
symbol extraction. Example (Go): it already says a `method_declaration`'s `@name` is the
`field_identifier` after the receiver, that a `function_declaration` is `@definition.function`, and
which nodes are `@reference.call`. So we don't hand-write extraction per language — **we run each
language's own `tags.scm` through one generic extractor.** That single fact makes language support
declarative.

Captures we use:
- `@definition.{function,method,class,interface,type,constant,module,…}` → the symbol + its kind
- `@name` → the symbol's name (no more brittle header-string parsing; Go receivers / JS arrows are
  handled by the grammar's query, not by us)
- `@reference.{call,type,…}` → grammar-accurate references → a real per-language graph for `refs` /
  `impact` / `context` (replaces today's word-boundary approximation)

---

## 1. Target architecture

### 1.1 `LanguagePack` — one declarative struct per language
```rust
pub struct LanguagePack {
    pub name: &'static str,                       // "dart"
    pub extensions: &'static [&'static str],      // [".dart"]
    pub grammar: fn() -> tree_sitter::Language,   // lazy; only called when a file of this kind is seen
    pub tags_query: &'static str,                 // include_str!(".../dart/tags.scm")
    pub block_style: BlockStyle,                  // Braces | Indent  (cheap apply pre-check only)
    pub line_comment: &'static str,               // "//" | "#"
    pub keywords: &'static [&'static str],        // rename keyword-guard
}
```
Adding a language = add the `tree-sitter-<lang>` dep + **one** `LanguagePack` literal. Nothing else.

### 1.2 Registry — the single source of truth
```rust
pub static REGISTRY: &[&LanguagePack] = &[
    #[cfg(feature = "rust")]   &RUST,
    #[cfg(feature = "python")] &PYTHON,
    #[cfg(feature = "dart")]   &DART,
    // …
];

pub fn pack_for_path(path: &str) -> Option<&'static LanguagePack> {
    let ext = extension_of(path);
    REGISTRY.iter().copied().find(|p| p.extensions.contains(&ext))
}
```
`detect_lang`, `kind_of`, `ts_language`, `validate_reparse`, `lang_extensions`, the keyword-guard — all
collapse into "look up the pack and read a field." The `Lang` enum (a closed set) goes away.

### 1.3 One generic extractor (deletes all per-language logic)
```rust
fn extract(text: &str, pack: &LanguagePack) -> SymbolTable {
    let lang  = pack.language();        // cached via OnceLock
    let query = pack.compiled_tags();   // Query::new(lang, pack.tags_query), cached
    let tree  = parse(text, lang);
    // walk query matches: capture name "definition.<kind>" -> kind; "@name" -> name; node -> span
}
```
This single function handles Rust, Go (receivers), TS (interfaces), JS (arrows), Dart, Java, … because
each grammar's `tags.scm` encodes its own rules. It **replaces** `kind_of`, `name_from_header`, the JS
arrow special-case, the Go-receiver fix, etc.

### 1.4 Isolation — "only the Dart flow for a Dart project"
Three layers, strongest first:
1. **Routing (runtime):** extension → exactly one pack. A `.dart` file only ever touches the Dart
   grammar + Dart `tags.scm`. The Python pack is never consulted. (True today by extension; the
   registry makes it the *only* dispatch path.)
2. **Lazy init (runtime):** `grammar()` and the compiled tags `Query` are built **once, on first use**,
   behind `OnceLock`. A pure-Flutter session never constructs the Rust/Python grammar or query → zero
   CPU/memory for unused languages. Nothing "Python" is loaded for a Dart project.
3. **Build (compile-time, optional):** each pack is `#[cfg(feature = "<lang>")]`.
   `default = ["rust","python","js","ts","go"]`. A Flutter shop builds
   `--no-default-features --features dart` → a smaller binary that **links only Dart**. No Rust/Python
   grammar in the artifact at all.

### 1.5 What every new pack gets *for free*
All nine superpowers are built on the generic symbol table + tree-sitter, not per-language code. So the
moment a pack exists, that language instantly has: structural / lexical / semantic / `mode=ast` search,
`detail=context`, `detail=impact`, `refs`, repo-wide `rename`, anchored / insert / delete / `move`
edits, atomic batches, `undo`, read-by-path. **~6 lines of pack → all of it.** The apply path needs
nothing per-language: `has_parse_error` (tree-sitter) is already language-generic; only the optional
brace pre-check and the keyword-guard read a pack field.

---

## 2. Migration plan (keep all 96 tests green at every step)

- **Phase A — Registry + pack, migrate the current 5 langs, no behavior change.** Introduce
  `LanguagePack`/`REGISTRY`; route `detect_lang`/`ts_language`/`validate_reparse`/`lang_extensions`
  through it. Keep the existing extractor as-is for now. *Mechanical, low risk.*
- **Phase B — Generic `tags.scm` extractor.** Swap the hand-written `kind_of` + `name_from_header` +
  arrow/receiver hacks for the single tags-query extractor. This is where extraction becomes *perfect*
  (each grammar's own spec) and the special cases delete themselves. Heuristic scanner stays only as
  the no-grammar fallback. *Medium; the real work.*
- **Phase C — Grammar-accurate references.** Use `@reference.*` captures to power `refs`/`impact`/
  `context` per language (replaces the word-boundary approximation; still honestly `graph=partial`
  without an LSP, but much better).
- **Phase D — Add languages.** Dart, Java, C, C++, C#, Ruby, PHP, Kotlin, Swift, Bash, Lua, Scala,
  Elixir, … Each = `tree-sitter-<lang>` dep + a `LanguagePack` (`grammar` fn, `include_str!` its
  `tags.scm`, `keywords`, `block_style`) + 2 smoke tests. **~15 minutes each.**
- **Phase E — Cargo feature-gate** every pack for slim, per-stack builds.

A new contributor adding **Dart** after Phase B does exactly:
```
1. Cargo.toml:  tree-sitter-dart = "..."
2. vendor or include_str! its queries/tags.scm
3. packs.rs:    pub static DART: LanguagePack = LanguagePack {
                  name: "dart", extensions: &[".dart"],
                  grammar: || tree_sitter_dart::language(),
                  tags_query: include_str!("queries/dart-tags.scm"),
                  block_style: BlockStyle::Braces, line_comment: "//",
                  keywords: DART_KEYWORDS };
4. registry.rs: add `#[cfg(feature="dart")] &DART,`
5. tests:       a 6-line "class/function extract" smoke test
```
No engine/apply/incr changes. The Flutter flow is now first-class and isolated.

---

## 3. Honest caveats / scope

- **A few grammars lack `tags.scm`** (rare for popular langs). Then write a ~15-line query once — still
  declarative, still one place. (Most ship one, as verified for Rust/Py/JS/TS/Go.)
- **`tags.scm` gives definitions + references, not full type resolution.** Symbol extraction, search,
  rename, context, AST queries, and structural edits become *perfect* per language. *Cross-file
  go-to-def precision* ("which `foo` exactly") still wants an LSP — that's the separate optional
  sidecar, language-by-language, and unrelated to this refactor.
- **Grammar version drift:** pin `tree-sitter-<lang>` versions; the tags-query format is stable across
  recent tree-sitter.
- **Binary size:** each linked grammar adds ~0.2–1 MB. Feature gating (Phase E) keeps per-stack builds
  lean; the default "batteries" build stays a single drop-in binary.

---

## 4. Why this is the right shape

- **Low marginal cost:** the burden of a language is a declaration, not logic — so "every famous
  language" is tractable, and each one inherits all nine superpowers automatically.
- **Correctness by delegation:** we use the *language maintainers'* own extraction spec (`tags.scm`),
  so we're as right as the grammar — no vyer-side per-language bugs (the Go-receiver and JS-arrow
  fixes we hand-wrote become unnecessary).
- **True isolation:** routing + lazy-init means a Dart project runs only Dart; feature gating means it
  can *ship* only Dart.
- **No regression risk to the core:** the incremental warm core, freshness, apply, and all superpowers
  sit above the symbol table and are language-agnostic; this plan only changes how the symbol table is
  produced.

---

## 5. As-built (what actually shipped) + answers to your three questions

**What landed** (`crates/vyer-index/src/lib.rs`):
- `mod langpack` — the registry. Each language = a `LangPack { grammar: fn()->Language, tags: &str }`
  plus one line in `pack(lang)`. The 6 packs (Rust/Python/JS/TS/Go/Dart) each carry a small
  `@def.<kind>` / `@name` tags query — the *entire* per-language spec.
- One generic `extract()` runs the pack's query → `Item`s with exact names. It **replaced**
  `kind_of`, `collect`, `js_var_fn_name`, and `name_from_header`'s Go-receiver hack. Go methods and JS
  arrows now fall out of the grammar's own structure (`method_declaration name:` / `variable_declarator
  … value: (arrow_function)`), and their special-case code is gone.
- `Item` gained an optional `name` (the `@name` capture); the heuristic scanner still fills it via the
  header as a fallback for unregistered languages.

**Adding a language is now:** add `tree-sitter-<lang>` to Cargo, add a `Lang::X` variant + one
`detect_lang` extension line (vyer-incr), and a `LangPack` + one `pack()` arm + a ~3-line tags query
(vyer-index). That's it — every superpower works for it immediately. (Dart proved it end-to-end.)

### Q1 — "make sure isolation doesn't cause calling / cold-start issues"
Each pack's grammar and compiled query are built **lazily, once**, behind `OnceLock`
(`get_or_init`). Consequences:
- A **Dart-only** project never calls `python_lang()`/`rust_lang()` or compiles their queries — those
  `OnceLock`s stay empty. Zero CPU/memory for unused languages; nothing "Python" is loaded.
- **Cold start** = index each file once. The *first* file of a given language pays a one-time
  grammar+query build (~1 ms, measured); every file/call after reuses the cached pack. There is **no
  per-call or per-file re-init** — so isolation adds *no* cold-start cost; it removes it (you only ever
  build the grammars you actually touch).

### Q2 — "a Python project can still have HTML/CSS/JS/anything"
Isolation is **per-file, never per-project.** `detect_lang(path)` routes *each file* to its own pack by
extension. A repo with `server.py` + `widget.dart` + `app.js` + `styles.css` indexes each with the
right grammar simultaneously — there is no "project language" lock. Verified live: a mixed
Dart+Python+CSS repo found Dart symbols via the Dart grammar and Python symbols via the Python grammar
in the same session.

### Q3 — files with no grammar (CSS/HTML/JSON/Markdown/…)
They route to `Lang::Generic` → the zero-dependency heuristic scanner → **lexical search still works**
(grep-quality), they just don't get *structural* symbols. They never error and never block. Add a pack
later (e.g. CSS/HTML) and they upgrade to structural for free. Verified: lexical search over a `.css`
file works in the mixed repo.

**Optional next step (Phase E):** `#[cfg(feature = "<lang>")]` on each pack so a Flutter shop can build
`--no-default-features --features dart` and ship a binary that *links* only Dart. Not required for
correctness (routing already isolates at runtime); it's purely for a leaner per-stack artifact.
