//! ccx-incr — a minimal, real incremental query engine for the code-context
//! engine, plus a working symbol/outline extractor.
//!
//! This is a faithful, transparent, zero-dependency model of what the design
//! calls the "Salsa incremental warm core." Production swaps in the `salsa`
//! crate (for parallel evaluation + fine-grained early-cutoff) and `tree-sitter`
//! (for correct parsing) at the exact same `parse` / `symbols` / `outline`
//! interfaces — but the mechanism demonstrated here is the real one:
//!
//!   * file contents are INPUTS; `parse`, `symbols`, `outline`, `repo_outline`
//!     are DERIVED queries whose results are MEMOIZED;
//!   * `set_text` mutates an input and bumps a global revision (unchanged text
//!     is a no-op — "durability");
//!   * a derived result is reused iff the content hash it was computed from
//!     still matches the input's current hash, so **editing one file recomputes
//!     only that file's chain** — and a query issued right after a write sees
//!     the new value (read-after-write freshness).
//!
//! Recompute counters (`Stats`) make the selective-recomputation property
//! observable and testable.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

type Hash = u64;

fn hash_str(s: &str) -> Hash {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash as _, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Compute the byte offset at which each line of `text` starts. Element 0 is
/// always 0; there is exactly one element per line. A line ends at the byte
/// before the next element (or at `text.len()` for the last line), so the `\n`
/// itself is part of its line's span.
///
/// "Number of lines" follows the editor convention, not `str::lines()`:
/// - `""` → `[0]` (one empty line)
/// - `"a"` → `[0]` (one line, no terminator)
/// - `"a\n"` → `[0]` (one line; a trailing newline does not open a new line)
/// - `"a\nb"` → `[0, 2]` (two lines)
/// - `"a\n\n"` → `[0, 2]` (a line "a" and an empty line)
///
/// Single pass, O(n). Capacity is pre-reserved from a newline estimate to avoid
/// reallocations on large files. `u32` offsets are sufficient because the engine
/// caps indexed files at 1 MiB.
fn compute_line_starts(text: &str) -> Vec<u32> {
    let bytes = text.as_bytes();
    // One slot for line 0, plus one per newline that is followed by more bytes.
    let mut starts: Vec<u32> = Vec::with_capacity(bytes.len() / 32 + 1);
    starts.push(0);
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' && i + 1 < bytes.len() {
            starts.push((i + 1) as u32);
        }
        i += 1;
    }
    starts
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    /// SCRY-099: `.tsx` (TypeScript + JSX) needs its OWN grammar — the plain
    /// `typescript` grammar misparses JSX (`<Tag>` collides with a type assertion),
    /// silently mis-bounding/dropping React components.
    Tsx,
    Go,
    Dart,
    Java,
    Ruby,
    Swift,
    Kotlin,
    C,
    Cpp,
    CSharp,
    Php,
    Generic,
}

/// Detect a file's language from its path extension. Public so the apply path
/// can determine the language of a not-yet-indexed file it is about to create.
pub fn detect_lang(path: &str) -> Lang {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".rs") {
        Lang::Rust
    } else if lower.ends_with(".py") || lower.ends_with(".pyi") || lower.ends_with(".pyw") {
        // SCRY-104: `.pyi` (type stubs — typeshed/library stubs) and `.pyw` (Windows
        // GUI scripts) are Python; the Python grammar parses both.
        Lang::Python
    } else if lower.ends_with(".tsx") {
        // SCRY-099: `.tsx` routes to the TSX grammar (JSX-aware), distinct from `.ts`.
        Lang::Tsx
    } else if lower.ends_with(".ts") || lower.ends_with(".mts") || lower.ends_with(".cts") {
        // TypeScript gets its own grammar so typed `class`/`interface` bodies
        // parse (the JS grammar chokes on type annotations) — SCRY-020.
        // SCRY-100: `.mts`/`.cts` are ESM/CommonJS TypeScript (TS 4.7+), same grammar.
        Lang::TypeScript
    } else if lower.ends_with(".js")
        || lower.ends_with(".jsx")
        || lower.ends_with(".mjs")
        || lower.ends_with(".cjs")
    {
        // SCRY-100: `.cjs` is CommonJS JavaScript (alongside the existing `.mjs` ESM).
        Lang::JavaScript
    } else if lower.ends_with(".go") {
        Lang::Go
    } else if lower.ends_with(".dart") {
        Lang::Dart
    } else if lower.ends_with(".java") {
        Lang::Java
    } else if lower.ends_with(".rb") {
        Lang::Ruby
    } else if lower.ends_with(".swift") {
        Lang::Swift
    } else if lower.ends_with(".kt") || lower.ends_with(".kts") {
        Lang::Kotlin
    } else if lower.ends_with(".c") {
        Lang::C
    } else if lower.ends_with(".cpp")
        || lower.ends_with(".cc")
        || lower.ends_with(".cxx")
        || lower.ends_with(".hpp")
        // SCRY-103: `.h`/`.hh`/`.hxx` are AMBIGUOUS (C or C++). Route them to the C++
        // grammar, which parses C headers too (C is ~a subset) AND extracts C++
        // headers' class/namespace/template — the C grammar silently drops those.
        || lower.ends_with(".h")
        || lower.ends_with(".hh")
        || lower.ends_with(".hxx")
    {
        Lang::Cpp
    } else if lower.ends_with(".cs") {
        Lang::CSharp
    } else if lower.ends_with(".php") {
        Lang::Php
    } else {
        Lang::Generic
    }
}

/// SCRY-106: extension-based detection, falling back to a `#!` shebang for
/// EXTENSIONLESS executables (common in `bin/`/`scripts/` — `deploy`, `manage`, …).
/// The extension always wins; the shebang is consulted only when it yields Generic,
/// so a real `.py`/`.js` file is never reinterpreted. bash/sh shebangs stay Generic
/// (no tree-sitter grammar here).
pub fn detect_lang_with_text(path: &str, text: &str) -> Lang {
    let by_ext = detect_lang(path);
    if by_ext != Lang::Generic {
        return by_ext;
    }
    shebang_lang(text).unwrap_or(Lang::Generic)
}

fn shebang_lang(text: &str) -> Option<Lang> {
    let first = text.lines().next()?;
    if !first.starts_with("#!") {
        return None;
    }
    let lower = first.to_ascii_lowercase();
    if lower.contains("python") {
        Some(Lang::Python)
    } else if lower.contains("node") {
        Some(Lang::JavaScript)
    } else if lower.contains("ruby") {
        Some(Lang::Ruby)
    } else {
        None
    }
}

// ---- derived value types -------------------------------------------------

/// A parse "tree": the structural step (boundaries of definitions). In
/// production this is a real tree-sitter tree; here it's the item spans we then
/// name in `symbols`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParseTree {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub kind: &'static str, // fn|struct|enum|trait|impl|mod|class|def|function
    pub header: String,     // the header/signature line, trimmed
    pub start: u32,         // 1-based
    pub end: u32,
    /// The symbol's name when the parser knows it exactly (e.g. a tree-sitter
    /// `@name` capture). `None` → derive it from `header` (heuristic fallback).
    /// Lets language packs supply correct names declaratively, no per-language
    /// string-parsing quirks (Go receivers, JS arrows) in the extractor.
    pub name: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SymbolTable {
    pub symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub kind: &'static str,
    pub name: String,
    pub signature: String,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// Signatures only, bodies elided — the cheap structural view.
    pub lines: Vec<String>,
}

// ---- the incremental database -------------------------------------------

struct Input {
    text: Arc<str>,
    hash: Hash,
    lang: Lang,
}

struct Memo<T> {
    value: T,
    input_hash: Hash, // the result is valid iff this == the input's current hash
}

#[derive(Default)]
pub struct Stats {
    pub parses: Cell<u64>,
    pub symbol_extractions: Cell<u64>,
    pub outline_builds: Cell<u64>,
    pub repo_builds: Cell<u64>,
    /// Recompute count for the line-offset index. Kept out of `snapshot()`'s
    /// 4-tuple (whose shape several tests assert literally); read directly via
    /// `stats.line_index_builds.get()` to prove the index is memoized.
    pub line_index_builds: Cell<u64>,
}
impl Stats {
    fn bump(c: &Cell<u64>) {
        c.set(c.get() + 1);
    }
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.parses.get(),
            self.symbol_extractions.get(),
            self.outline_builds.get(),
            self.repo_builds.get(),
        )
    }
}

pub struct Db {
    inputs: HashMap<String, Input>,
    revision: u64,
    parse_memo: RefCell<HashMap<String, Memo<Arc<ParseTree>>>>,
    symbols_memo: RefCell<HashMap<String, Memo<Arc<SymbolTable>>>>,
    outline_memo: RefCell<HashMap<String, Memo<Arc<Outline>>>>,
    /// DERIVED: byte offset of every line start (the line-offset index). Keyed
    /// and validated by content hash exactly like the other memos, so a
    /// line-range read is O(range) byte-copy instead of an O(n) newline scan.
    line_index_memo: RefCell<HashMap<String, Memo<Arc<Vec<u32>>>>>,
    repo_memo: RefCell<Option<(Hash, RepoOutline)>>,
    /// Optional pluggable parser. When set (e.g. the tree-sitter parser injected
    /// by `vyer-index`), it replaces the built-in heuristic scanner at the exact
    /// `parse` call site — so the incremental spine, memoization, and freshness
    /// are untouched while parsing becomes AST-accurate. Keeping this an injected
    /// closure is what lets `vyer-incr` stay zero-dependency (Rule §8).
    parser: Option<Parser>,
    pub stats: Stats,
}

/// All files' outlines, in sorted path order — the repo-level derived view.
pub type RepoOutline = Arc<Vec<(String, Arc<Outline>)>>;

/// A pluggable parser: text + language → structural item spans.
pub type Parser = Arc<dyn Fn(&str, Lang) -> ParseTree + Send + Sync>;

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

impl Db {
    pub fn new() -> Self {
        Db {
            inputs: HashMap::new(),
            revision: 0,
            parse_memo: RefCell::new(HashMap::new()),
            symbols_memo: RefCell::new(HashMap::new()),
            outline_memo: RefCell::new(HashMap::new()),
            line_index_memo: RefCell::new(HashMap::new()),
            repo_memo: RefCell::new(None),
            parser: None,
            stats: Stats::default(),
        }
    }

    /// Install a parser that overrides the built-in heuristic scanner. Doing so
    /// invalidates parse-derived memos (their next read recomputes with the new
    /// parser). Call before indexing for best effect.
    pub fn set_parser(&mut self, parser: Parser) {
        self.parser = Some(parser);
        self.parse_memo.borrow_mut().clear();
        self.symbols_memo.borrow_mut().clear();
        self.outline_memo.borrow_mut().clear();
        *self.repo_memo.borrow_mut() = None;
        self.revision += 1;
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inputs.keys().cloned().collect();
        v.sort();
        v
    }

    /// SCRY-079: the content hash of a file's current text, if indexed. Lets a
    /// derived index (e.g. the server's token index) detect which files changed
    /// since it last built — so it can update incrementally instead of rebuilding
    /// the whole repo after every edit, without re-reading any text.
    pub fn content_hash(&self, path: &str) -> Option<u64> {
        self.inputs.get(path).map(|i| i.hash)
    }

    /// Set (or update) a file's contents. Unchanged text is a no-op and does
    /// NOT bump the revision (durability). This is the only input mutation; it
    /// is synchronous, so a query issued afterwards sees the new value
    /// (read-after-write freshness).
    pub fn set_text(&mut self, path: &str, text: &str) {
        let hash = hash_str(text);
        if let Some(existing) = self.inputs.get(path) {
            if existing.hash == hash {
                return; // durability: nothing changed
            }
        }
        self.inputs.insert(
            path.to_string(),
            Input {
                text: Arc::from(text),
                hash,
                lang: detect_lang_with_text(path, text),
            },
        );
        self.revision += 1;
        // Stale memos are validated lazily by hash on next read; we don't eagerly
        // purge. (A production engine also drops obviously-dead entries.)
    }

    /// Drop a file from the index (after it is deleted on disk). Bumps the
    /// revision so derived/repo views refresh; memos for the path go stale and
    /// are never read again. Keeps read-after-write freshness for deletions.
    pub fn remove_text(&mut self, path: &str) -> bool {
        let existed = self.inputs.remove(path).is_some();
        if existed {
            self.parse_memo.borrow_mut().remove(path);
            self.symbols_memo.borrow_mut().remove(path);
            self.outline_memo.borrow_mut().remove(path);
            self.line_index_memo.borrow_mut().remove(path);
            *self.repo_memo.borrow_mut() = None;
            self.revision += 1;
        }
        existed
    }

    fn input(&self, path: &str) -> Option<&Input> {
        self.inputs.get(path)
    }

    /// The file's current text, if indexed. This is the single source of truth
    /// that lexical search and the apply path read, so a read right after a
    /// write observes the new content (read-after-write freshness).
    pub fn text(&self, path: &str) -> Option<Arc<str>> {
        self.inputs.get(path).map(|i| i.text.clone())
    }

    /// The detected language for an indexed file (`Generic` if unknown/unindexed).
    pub fn lang(&self, path: &str) -> Lang {
        self.inputs
            .get(path)
            .map(|i| i.lang)
            .unwrap_or(Lang::Generic)
    }

    /// DERIVED: structural parse. Memoized; recomputed only when the file's
    /// content hash changes.
    ///
    /// Used as a graceful fallback by an injected parser (e.g. `vyer-index`)
    /// when a real parse fails — degrade to the heuristic, never crash.
    pub fn heuristic_parse(text: &str, lang: Lang) -> ParseTree {
        lang::parse_text(text, lang)
    }

    pub fn parse(&self, path: &str) -> Arc<ParseTree> {
        let input = match self.input(path) {
            Some(i) => i,
            None => return Arc::new(ParseTree::default()),
        };
        if let Some(m) = self.parse_memo.borrow().get(path) {
            if m.input_hash == input.hash {
                return m.value.clone(); // memo hit — no recompute
            }
        }
        Stats::bump(&self.stats.parses);
        let tree = Arc::new(match &self.parser {
            Some(p) => p(&input.text, input.lang),
            None => lang::parse_text(&input.text, input.lang),
        });
        self.parse_memo.borrow_mut().insert(
            path.to_string(),
            Memo {
                value: tree.clone(),
                input_hash: input.hash,
            },
        );
        tree
    }

    /// DERIVED: symbol table (depends on `parse`).
    pub fn symbols(&self, path: &str) -> Arc<SymbolTable> {
        let input_hash = match self.input(path) {
            Some(i) => i.hash,
            None => return Arc::new(SymbolTable::default()),
        };
        if let Some(m) = self.symbols_memo.borrow().get(path) {
            if m.input_hash == input_hash {
                return m.value.clone();
            }
        }
        let tree = self.parse(path); // dependency; hits its own memo if fresh
        let lang = self.input(path).map(|i| i.lang).unwrap_or(Lang::Generic);
        Stats::bump(&self.stats.symbol_extractions);
        let st = Arc::new(lang::extract_symbols(&tree, lang));
        self.symbols_memo.borrow_mut().insert(
            path.to_string(),
            Memo {
                value: st.clone(),
                input_hash,
            },
        );
        st
    }

    /// DERIVED: outline / signatures-only view (depends on `symbols`).
    pub fn outline(&self, path: &str) -> Arc<Outline> {
        let input_hash = match self.input(path) {
            Some(i) => i.hash,
            None => return Arc::new(Outline::default()),
        };
        if let Some(m) = self.outline_memo.borrow().get(path) {
            if m.input_hash == input_hash {
                return m.value.clone();
            }
        }
        let syms = self.symbols(path);
        Stats::bump(&self.stats.outline_builds);
        let lines = syms
            .symbols
            .iter()
            .map(|s| format!("{} @L{}-{}", s.signature, s.start, s.end))
            .collect();
        let outline = Arc::new(Outline { lines });
        self.outline_memo.borrow_mut().insert(
            path.to_string(),
            Memo {
                value: outline.clone(),
                input_hash,
            },
        );
        outline
    }

    /// DERIVED: the line-offset index — the byte offset at which each line
    /// starts. `index[0]` is always 0; `index.len()` is the number of lines.
    /// Line `i` (1-based) spans bytes `[index[i-1] .. index.get(i).copied()
    /// .unwrap_or(text.len())]`. Memoized and hash-validated like the other
    /// derived queries, so it is built once per file version and reused.
    ///
    /// Independent of the parser (pure text), so unlike `parse`/`symbols` it is
    /// NOT cleared by `set_parser` — only by an actual content change (the hash
    /// guard) or `remove_text`.
    ///
    /// Why this exists: a line-range read becomes an O(1) offset lookup plus an
    /// O(range) byte copy, instead of an O(n) newline scan that also allocates a
    /// `Vec<&str>` of every line. Slicing on these offsets can never split a
    /// UTF-8 character: every boundary is a line start (just past a `\n`) or EOF,
    /// and `\n` (0x0A) cannot appear inside a multi-byte UTF-8 sequence.
    pub fn line_index(&self, path: &str) -> Arc<Vec<u32>> {
        let input = match self.input(path) {
            Some(i) => i,
            None => return Arc::new(vec![0]),
        };
        if let Some(m) = self.line_index_memo.borrow().get(path) {
            if m.input_hash == input.hash {
                return m.value.clone();
            }
        }
        Stats::bump(&self.stats.line_index_builds);
        let idx = Arc::new(compute_line_starts(&input.text));
        self.line_index_memo.borrow_mut().insert(
            path.to_string(),
            Memo {
                value: idx.clone(),
                input_hash: input.hash,
            },
        );
        idx
    }

    /// DERIVED (repo-level): outlines for all files. Depends on the multiset of
    /// file content hashes, so it is reused unless some file changed — and when
    /// one file changes, only that file's `outline` recomputes underneath (the
    /// others hit their memos).
    pub fn repo_outline(&self) -> RepoOutline {
        let mut pairs: Vec<(String, Hash)> = self
            .inputs
            .iter()
            .map(|(p, i)| (p.clone(), i.hash))
            .collect();
        pairs.sort();
        let combined = {
            let mut s = String::new();
            for (p, h) in &pairs {
                s.push_str(p);
                s.push(':');
                s.push_str(&h.to_string());
                s.push('\n');
            }
            hash_str(&s)
        };
        if let Some((h, v)) = self.repo_memo.borrow().as_ref() {
            if *h == combined {
                return v.clone();
            }
        }
        Stats::bump(&self.stats.repo_builds);
        let mut out = Vec::with_capacity(pairs.len());
        for (p, _h) in &pairs {
            out.push((p.clone(), self.outline(p))); // hits per-file memos when fresh
        }
        let arc = Arc::new(out);
        *self.repo_memo.borrow_mut() = Some((combined, arc.clone()));
        arc
    }
}

// ---- the (real, modest) language extractors ------------------------------
// Not as complete as tree-sitter, but real: they extract function/type/class
// definitions from actual source for Rust, Python, and JS/TS. The `parse` step
// finds item boundaries; `extract_symbols` names and cleans the signatures.
mod lang {
    use super::*;

    pub fn parse_text(text: &str, lang: Lang) -> ParseTree {
        match lang {
            Lang::Rust => parse_brace(text, &["fn", "struct", "enum", "trait", "impl", "mod"]),
            Lang::JavaScript => parse_brace(text, &["function", "class"]),
            Lang::TypeScript | Lang::Tsx => {
                parse_brace(text, &["function", "class", "interface", "enum", "type"])
            }
            Lang::Go => parse_brace(text, &["func", "type"]),
            Lang::Dart => parse_brace(text, &["class", "void", "int", "String", "Future"]),
            Lang::Java => parse_brace(
                text,
                &["class", "interface", "enum", "void", "public", "private"],
            ),
            Lang::Ruby => parse_brace(text, &["def", "class", "module"]),
            Lang::Swift => parse_brace(text, &["class", "struct", "func", "protocol", "enum"]),
            Lang::Kotlin => parse_brace(text, &["class", "fun", "object", "interface"]),
            Lang::C => parse_brace(text, &["struct", "enum", "union", "void", "int", "char"]),
            Lang::Cpp => parse_brace(
                text,
                &["class", "struct", "namespace", "void", "int", "template"],
            ),
            Lang::CSharp => parse_brace(
                text,
                &["class", "interface", "struct", "enum", "namespace", "void"],
            ),
            Lang::Php => parse_brace(text, &["function", "class", "interface", "trait"]),
            Lang::Python => parse_python(text),
            Lang::Generic => ParseTree::default(),
        }
    }

    /// Detect items by a leading keyword (after optional modifiers) and find the
    /// end via naive brace matching, or `;` for a bodyless declaration. Good
    /// enough for a reference; tree-sitter handles strings/comments correctly.
    fn parse_brace(text: &str, keywords: &[&'static str]) -> ParseTree {
        let lines: Vec<&str> = text.lines().collect();
        let mut items = Vec::new();
        let mut i = 0usize;
        while i < lines.len() {
            let trimmed = strip_modifiers(lines[i].trim());
            let kw_opt: Option<&'static str> = keywords
                .iter()
                .copied()
                .find(|k| starts_with_word(trimmed, k));
            if let Some(kw) = kw_opt {
                let start = (i as u32) + 1;
                // find end: brace matching from this line forward
                let mut depth: i32 = 0;
                let mut seen_brace = false;
                let mut end = start;
                let mut j = i;
                let mut decl_only = false;
                'scan: while j < lines.len() {
                    for ch in lines[j].chars() {
                        match ch {
                            '{' => {
                                depth += 1;
                                seen_brace = true;
                            }
                            '}' => {
                                depth -= 1;
                                if seen_brace && depth == 0 {
                                    end = (j as u32) + 1;
                                    break 'scan;
                                }
                            }
                            ';' if !seen_brace => {
                                // bodyless decl (e.g. trait method, struct;)
                                end = (j as u32) + 1;
                                decl_only = true;
                                break 'scan;
                            }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                if !seen_brace && !decl_only {
                    end = start; // single line fallback
                }
                items.push(Item {
                    kind: kw,
                    header: trimmed.to_string(),
                    start,
                    end,
                    name: None,
                });
                i = (end as usize).max(i + 1);
            } else {
                i += 1;
            }
        }
        ParseTree { items }
    }

    /// Python: `def`/`class` by indentation; end is the next non-blank line whose
    /// indentation is <= the header's.
    fn parse_python(text: &str) -> ParseTree {
        let lines: Vec<&str> = text.lines().collect();
        let mut items = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let kind = if starts_with_word(trimmed, "def") {
                Some("def")
            } else if starts_with_word(trimmed, "class") {
                Some("class")
            } else {
                None
            };
            if let Some(kind) = kind {
                let indent = indent_of(line);
                let start = (idx as u32) + 1;
                let mut end = start;
                for (k, l2) in lines.iter().enumerate().skip(idx + 1) {
                    if l2.trim().is_empty() {
                        continue;
                    }
                    if indent_of(l2) <= indent {
                        break;
                    }
                    end = (k as u32) + 1;
                }
                items.push(Item {
                    kind,
                    header: trimmed.to_string(),
                    start,
                    end,
                    name: None,
                });
            }
        }
        items
            .into_iter()
            .collect::<Vec<_>>()
            .pipe(|items| ParseTree { items })
    }

    pub fn extract_symbols(tree: &ParseTree, _lang: Lang) -> SymbolTable {
        let symbols = tree
            .items
            .iter()
            .map(|it| {
                // Prefer the parser-supplied name (tree-sitter @name); fall back
                // to deriving it from the header line for the heuristic scanner.
                let name = it
                    .name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| name_from_header(it.kind, &it.header));
                let signature = clean_signature(&it.header);
                Symbol {
                    kind: it.kind,
                    name,
                    signature,
                    start: it.start,
                    end: it.end,
                }
            })
            .collect();
        SymbolTable { symbols }
    }

    // --- small helpers ---

    fn strip_modifiers(s: &str) -> &str {
        let mut s = s;
        for m in [
            "pub(crate)",
            "pub",
            "async",
            "unsafe",
            "default",
            "export",
            "static",
            "abstract",
            "declare",
        ] {
            if let Some(rest) = s.strip_prefix(m) {
                if rest.starts_with(' ') {
                    s = rest.trim_start();
                }
            }
        }
        s
    }

    fn starts_with_word(s: &str, w: &str) -> bool {
        if let Some(rest) = s.strip_prefix(w) {
            rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric() && c != '_')
        } else {
            false
        }
    }

    fn indent_of(s: &str) -> usize {
        s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
    }

    fn name_from_header(kind: &str, header: &str) -> String {
        let h = strip_modifiers(header);
        // drop the keyword
        let after = h.strip_prefix(kind).map(|x| x.trim_start()).unwrap_or(h);
        // Go method: `func (recv T) Name(...)` — the receiver group comes before
        // the name, so skip a leading `(...)` to land on the actual method name
        // (without this, Go methods extract an empty name and are unsearchable).
        let after = if after.starts_with('(') {
            match after.find(')') {
                Some(i) => after[i + 1..].trim_start(),
                None => after,
            }
        } else {
            after
        };
        // name is up to the first delimiter
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        name
    }

    fn clean_signature(header: &str) -> String {
        // trim a trailing opening brace / colon for a tidy signature line
        let h = header.trim_end();
        h.trim_end_matches('{')
            .trim_end_matches(':')
            .trim_end()
            .to_string()
    }

    // tiny pipe helper to keep parse_python readable without extra deps
    trait Pipe: Sized {
        fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
            f(self)
        }
    }
    impl<T> Pipe for T {}
}

// ---------------------------------------------------------------------------
// Tests — these PROVE the architectural claims. Run with `cargo test`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_ts_js_extensions_route_to_the_right_grammar() {
        // SCRY-099/100: real-world TS/JS extensions must map to the correct language —
        // `.tsx` to the JSX-aware grammar, `.mts`/`.cts` (ESM/CJS TS) to TypeScript,
        // `.cjs` (CommonJS JS) to JavaScript. A miss leaves them at `Generic` (no symbols).
        assert_eq!(detect_lang("a.tsx"), Lang::Tsx);
        assert_eq!(detect_lang("a.ts"), Lang::TypeScript);
        assert_eq!(detect_lang("a.mts"), Lang::TypeScript);
        assert_eq!(detect_lang("a.cts"), Lang::TypeScript);
        assert_eq!(detect_lang("a.cjs"), Lang::JavaScript);
        assert_eq!(detect_lang("a.mjs"), Lang::JavaScript);
        assert_eq!(detect_lang("a.jsx"), Lang::JavaScript);
    }

    #[test]
    fn c_cpp_header_extensions_route_to_the_right_grammar() {
        // SCRY-103: `.h`/`.hh`/`.hxx` are ambiguous (C or C++). Route them to the C++
        // grammar — it parses C headers too (C is ~a subset) AND extracts C++ headers'
        // class/namespace/template, which the C grammar silently drops. `.c` stays C.
        assert_eq!(detect_lang("a.c"), Lang::C);
        assert_eq!(detect_lang("a.h"), Lang::Cpp);
        assert_eq!(detect_lang("a.hh"), Lang::Cpp);
        assert_eq!(detect_lang("a.hpp"), Lang::Cpp);
        assert_eq!(detect_lang("a.cpp"), Lang::Cpp);
    }

    #[test]
    fn python_stub_and_gui_extensions_route_to_python() {
        // SCRY-104: `.pyi` (type stubs — typeshed/library stubs) and `.pyw` (Windows
        // GUI scripts) are Python; without this they fall to Generic (no symbols).
        assert_eq!(detect_lang("a.py"), Lang::Python);
        assert_eq!(detect_lang("a.pyi"), Lang::Python);
        assert_eq!(detect_lang("a.pyw"), Lang::Python);
    }

    #[test]
    fn shebang_detects_language_for_extensionless_scripts() {
        // SCRY-106: an extensionless executable (bin/scripts) gets its language from
        // its `#!` shebang; a real extension always wins over the shebang.
        assert_eq!(
            detect_lang_with_text("scripts/deploy", "#!/usr/bin/env python3\ndef f(): pass\n"),
            Lang::Python
        );
        assert_eq!(
            detect_lang_with_text("scripts/build", "#!/usr/bin/env node\nfunction g(){}\n"),
            Lang::JavaScript
        );
        assert_eq!(
            detect_lang_with_text("scripts/task", "#!/usr/bin/env ruby\ndef h; end\n"),
            Lang::Ruby
        );
        // a real extension WINS over a (misleading) shebang:
        assert_eq!(
            detect_lang_with_text("a.rs", "#!/usr/bin/env python\nfn x(){}\n"),
            Lang::Rust
        );
        // bash/sh stay Generic (no tree-sitter grammar here); no shebang → Generic.
        assert_eq!(
            detect_lang_with_text("scripts/run", "#!/bin/bash\necho hi\n"),
            Lang::Generic
        );
        assert_eq!(
            detect_lang_with_text("README", "just text\n"),
            Lang::Generic
        );
    }

    const RUST_A: &str = "\
pub fn validate_token(tok: &str) -> Result<Claims> {
    let c = parse(tok)?;
    Ok(c)
}

struct Session {
    id: u64,
}
";

    const RUST_B: &str = "\
pub fn login(req: LoginReq) -> Result<Session> {
    Ok(Session { id: 1 })
}
";

    #[test]
    fn symbols_are_extracted_from_real_rust() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        let syms = db.symbols("a.rs");
        let names: Vec<&str> = syms.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"validate_token"), "names were {:?}", names);
        assert!(names.contains(&"Session"));
        // line span of the function is correct (1..=4)
        let f = syms
            .symbols
            .iter()
            .find(|s| s.name == "validate_token")
            .unwrap();
        assert_eq!((f.start, f.end), (1, 4));
        assert_eq!(f.kind, "fn");
    }

    #[test]
    fn python_and_js_extract_too() {
        let mut db = Db::new();
        db.set_text("m.py", "class Foo:\n    def bar(self):\n        return 1\n");
        let py = db.symbols("m.py");
        let names: Vec<&str> = py.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));

        db.set_text("x.ts", "export function add(a, b) {\n  return a + b;\n}\n");
        let js = db.symbols("x.ts");
        assert!(js
            .symbols
            .iter()
            .any(|s| s.name == "add" && s.kind == "function"));
    }

    #[test]
    fn go_method_receiver_name_is_extracted() {
        // Regression for SCRY-020: a Go method `func (recv T) Name()` must
        // resolve to `Name`, not the empty string (receiver was swallowing it).
        let mut db = Db::new();
        db.set_text(
            "m.go",
            "package p\nfunc (s Shape) Area() int { return s.side }\nfunc Free() int { return 1 }\n",
        );
        let names: Vec<String> = db
            .symbols("m.go")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"Area".to_string()), "names: {names:?}");
        assert!(names.contains(&"Free".to_string()), "names: {names:?}");
    }

    #[test]
    fn first_query_computes_chain_once() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        let _ = db.outline("a.rs");
        assert_eq!(db.stats.snapshot(), (1, 1, 1, 0)); // parse, symbols, outline once
    }

    #[test]
    fn repeated_query_is_a_memo_hit() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        let _ = db.outline("a.rs");
        let before = db.stats.snapshot();
        let _ = db.outline("a.rs"); // no edit
        let _ = db.symbols("a.rs");
        let _ = db.parse("a.rs");
        assert_eq!(
            db.stats.snapshot(),
            before,
            "no recompute expected on cache hit"
        );
    }

    #[test]
    fn unchanged_set_text_does_not_bump_revision_or_recompute() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        let _ = db.outline("a.rs");
        let rev = db.revision();
        let before = db.stats.snapshot();
        db.set_text("a.rs", RUST_A); // identical content
        assert_eq!(db.revision(), rev, "durability: no revision bump");
        let _ = db.outline("a.rs");
        assert_eq!(
            db.stats.snapshot(),
            before,
            "no recompute after no-op write"
        );
    }

    #[test]
    fn edit_recomputes_only_that_files_chain() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        db.set_text("b.rs", RUST_B);
        // warm both
        let _ = db.outline("a.rs");
        let _ = db.outline("b.rs");
        let before = db.stats.snapshot(); // (2,2,2,0)
        assert_eq!(before, (2, 2, 2, 0));

        // edit ONLY a.rs
        db.set_text("a.rs", &format!("{}\n// touched\n", RUST_A));

        // querying b.rs must NOT recompute anything (selective invalidation)
        let _ = db.outline("b.rs");
        assert_eq!(
            db.stats.snapshot(),
            before,
            "b.rs must stay cached after a.rs edit"
        );

        // querying a.rs recomputes its chain exactly once more
        let _ = db.outline("a.rs");
        assert_eq!(db.stats.snapshot(), (3, 3, 3, 0));
    }

    #[test]
    fn repo_outline_rebuilds_only_when_a_file_changes_and_reuses_unchanged() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        db.set_text("b.rs", RUST_B);
        let _ = db.repo_outline();
        let after_first = db.stats.snapshot();
        // second call, no change => repo memo hit, zero recompute
        let _ = db.repo_outline();
        assert_eq!(db.stats.snapshot(), after_first);

        // change a.rs => repo rebuild, but only a.rs's outline recomputes underneath
        let (_p0, _s0, o0, r0) = db.stats.snapshot();
        db.set_text("a.rs", &format!("{}\nfn extra() {{}}\n", RUST_A));
        let _ = db.repo_outline();
        let (_p1, _s1, o1, r1) = db.stats.snapshot();
        assert_eq!(r1, r0 + 1, "exactly one repo rebuild");
        assert_eq!(
            o1,
            o0 + 1,
            "exactly one per-file outline recompute (a.rs only)"
        );
    }

    // ----- line-offset index ------------------------------------------------

    /// Resolve line `n` (1-based) of `text` via the index, exactly as a reader
    /// would, so the tests assert end-to-end slicing — not just the raw offsets.
    fn line_via_index(db: &Db, path: &str, n: u32) -> String {
        let idx = db.line_index(path);
        let text = db.text(path).unwrap();
        let start = idx[(n - 1) as usize] as usize;
        let end = idx
            .get(n as usize)
            .map(|o| *o as usize)
            .unwrap_or(text.len());
        text[start..end].trim_end_matches(['\n', '\r']).to_string()
    }

    #[test]
    fn line_index_offsets_and_counts_follow_editor_convention() {
        let cases: &[(&str, &[u32])] = &[
            ("", &[0]),
            ("a", &[0]),
            ("a\n", &[0]),
            ("a\nb", &[0, 2]),
            ("a\nb\n", &[0, 2]),
            ("a\n\n", &[0, 2]), // "a", then an empty line
            ("\n\n", &[0, 1]),  // empty line, then empty line
        ];
        let mut db = Db::new();
        for (text, want) in cases {
            db.set_text("f.txt", text);
            assert_eq!(
                db.line_index("f.txt").as_slice(),
                *want,
                "line starts for {text:?}"
            );
        }
    }

    #[test]
    fn line_index_slices_never_split_utf8_and_round_trip() {
        let mut db = Db::new();
        // multi-byte content on several lines (é = 2 bytes, 中 = 3, 😀 = 4)
        let text = "café\nαβγ\n中文行\n😀😀\nlast";
        db.set_text("u.txt", text);
        let idx = db.line_index("u.txt");
        assert_eq!(idx.len(), 5, "five lines");
        // every offset is a valid char boundary (slicing can't panic)
        for &o in idx.iter() {
            assert!(text.is_char_boundary(o as usize));
        }
        assert_eq!(line_via_index(&db, "u.txt", 1), "café");
        assert_eq!(line_via_index(&db, "u.txt", 3), "中文行");
        assert_eq!(line_via_index(&db, "u.txt", 4), "😀😀");
        assert_eq!(line_via_index(&db, "u.txt", 5), "last");
    }

    #[test]
    fn line_index_handles_crlf() {
        let mut db = Db::new();
        db.set_text("w.txt", "one\r\ntwo\r\nthree");
        // offsets key on \n; the \r stays inside the line and is trimmed on read
        assert_eq!(db.line_index("w.txt").as_slice(), &[0, 5, 10]);
        assert_eq!(line_via_index(&db, "w.txt", 2), "two");
    }

    #[test]
    fn line_index_is_memoized_and_hash_validated() {
        let mut db = Db::new();
        db.set_text("a.rs", RUST_A);
        let _ = db.line_index("a.rs");
        let after_first = db.stats.line_index_builds.get();
        assert_eq!(after_first, 1, "built once");

        // repeat + no-op write => memo hit, no rebuild
        let _ = db.line_index("a.rs");
        db.set_text("a.rs", RUST_A);
        let _ = db.line_index("a.rs");
        assert_eq!(db.stats.line_index_builds.get(), after_first, "no rebuild");

        // real edit => exactly one rebuild
        db.set_text("a.rs", &format!("{RUST_A}\n// touched\n"));
        let _ = db.line_index("a.rs");
        assert_eq!(db.stats.line_index_builds.get(), after_first + 1);
    }

    #[test]
    fn line_index_of_unindexed_file_is_safe() {
        let db = Db::new();
        assert_eq!(db.line_index("missing.rs").as_slice(), &[0]);
    }
}
