//! The deterministic apply path — Vyer's reason for existing past "search".
//!
//! Editing, not localization, is the real bottleneck (a frontier full-file
//! rewrite is ~100s and dollars). So the primary path is *deterministic*: anchor
//! to a symbol's node, splice the new body over exactly its line span, then
//! **re-validate by re-parsing** and reject if the result is structurally broken
//! — never a silent bad write. A model (fast-apply) is only the fallback, and it
//! too must pass the same re-parse gate.
//!
//! This module is pure: it takes the current file text + the resolved symbol
//! span + the new body and returns either a `PreparedEdit` (new text + unified
//! diff) or a typed rejection. The engine performs the sandbox check, the write,
//! and the synchronous `set_text` that keeps the warm core fresh.

use vyer_core::output::sanitize_field;
use vyer_incr::{Lang, SymbolTable};

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The locator named a symbol the current file does not contain.
    SymbolNotFound { symbol: String },
    /// The locator was ambiguous (several symbols share the name) and no line
    /// range was given to disambiguate.
    Ambiguous {
        symbol: String,
        /// The matching symbols' line spans, so the error can show the agent
        /// exactly which `@L` to pick (SCRY-082) instead of just "pass a range".
        ranges: Vec<(u32, u32)>,
    },
    /// The spliced result does not re-parse (unbalanced delimiters) — rejected
    /// before any write so the tree is never left broken.
    ReparseFailed { reason: String },
    /// Neither `new_body` nor `lazy_edit` was provided.
    NoEdit,
    /// An anchored edit's `anchor` text was not found in the target scope.
    AnchorNotFound { anchor: String },
    /// An anchored edit's `anchor` matched more than once in scope — ambiguous,
    /// so we refuse rather than edit the wrong occurrence (add more context).
    AnchorAmbiguous { anchor: String, count: usize },
    /// SCRY-006: the `anchor` wasn't found verbatim, but a whitespace-normalized
    /// match exists in scope — almost always an indentation / tab-vs-space /
    /// trailing-whitespace difference. We name the likely cause instead of a bare
    /// "not found", turning a dead-end into a one-line fix.
    AnchorWhitespaceMismatch { anchor: String },
    /// SP-12: `@into:` was used on a container we can't safely splice into — a
    /// single-line block (no interior) or an unrecognized language. We refuse
    /// rather than misplace the member, and point at `@after:<member>` instead.
    ContainerInsertUnsupported { container: String },
}

/// Where to splice a freshly-authored symbol (SCRY-013).
#[derive(Debug, Clone)]
pub enum InsertPos {
    /// Immediately after the named symbol's node.
    After(String, Option<(u32, u32)>),
    /// Immediately before the named symbol's node.
    Before(String, Option<(u32, u32)>),
    /// At end of file.
    End,
    /// Inside the named container (class/impl/struct/…), just before its closing
    /// delimiter — for adding a method/field to an existing block (SP-12). The
    /// optional line range disambiguates same-named containers (e.g. a `struct`
    /// and its `impl`).
    Into(String, Option<(u32, u32)>),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::SymbolNotFound { symbol } => {
                write!(f, "no symbol `{symbol}` in file")
            }
            ApplyError::Ambiguous { symbol, ranges } => {
                let opts: Vec<String> = ranges
                    .iter()
                    .map(|(s, e)| format!("@L{s}-{e}"))
                    .collect();
                write!(
                    f,
                    "symbol `{symbol}` is ambiguous ({} matches); disambiguate by appending one to the locator: {}",
                    ranges.len(),
                    opts.join(" | ")
                )
            }
            ApplyError::ReparseFailed { reason } => {
                write!(f, "edit rejected: result does not parse ({reason})")
            }
            ApplyError::NoEdit => write!(f, "edit has neither new_body nor lazy_edit"),
            ApplyError::AnchorNotFound { anchor } => {
                write!(f, "anchor not found in scope: `{anchor}`")
            }
            ApplyError::AnchorAmbiguous { anchor, count } => write!(
                f,
                "anchor `{anchor}` matches {count} times in scope; add surrounding context to make it unique"
            ),
            ApplyError::AnchorWhitespaceMismatch { anchor } => write!(
                f,
                "anchor not found as written, but a whitespace-different match exists in scope — check indentation, tabs-vs-spaces, and trailing whitespace: `{anchor}`"
            ),
            ApplyError::ContainerInsertUnsupported { container } => write!(
                f,
                "cannot @into `{container}`: only a multi-line container block (class/impl/struct/…) is supported — a single-line block or an unrecognized language can't be spliced safely; use @after:<a member of {container}> instead"
            ),
        }
    }
}

/// A validated, ready-to-write edit. The engine writes `new_text` and feeds it
/// straight back into the incremental core.
#[derive(Debug, Clone)]
pub struct PreparedEdit {
    pub new_text: String,
    pub diff: String,
    /// 1-based inclusive line span that was replaced (for the audit log).
    pub start: u32,
    pub end: u32,
}

/// Resolve which symbol the edit targets. `want_range` (start,end) disambiguates
/// when several symbols share a name. Accepts a class-qualified name
/// `Container.method` / `Container::method` (SCRY-005) — the member is the
/// same-named symbol nested inside the container's span, which disambiguates
/// same-named methods across classes without needing line numbers.
fn resolve_span(
    syms: &SymbolTable,
    symbol: &str,
    want_range: Option<(u32, u32)>,
) -> Result<(u32, u32), ApplyError> {
    // Class-qualified path first (`A::b` Rust-style, or `A.b`).
    let qualified = symbol.split_once("::").or_else(|| symbol.rsplit_once('.'));
    if let Some((container, member)) = qualified {
        let containers: Vec<&vyer_incr::Symbol> = syms
            .symbols
            .iter()
            .filter(|s| s.name == container)
            .collect();
        let hits: Vec<&vyer_incr::Symbol> = syms
            .symbols
            .iter()
            .filter(|s| {
                s.name == member
                    && containers.iter().any(|c| {
                        s.start >= c.start && s.end <= c.end && (s.start, s.end) != (c.start, c.end)
                    })
            })
            .collect();
        return match hits.as_slice() {
            [] => Err(ApplyError::SymbolNotFound {
                symbol: symbol.to_string(),
            }),
            [only] => Ok((only.start, only.end)),
            many => {
                if let Some((ws, we)) = want_range {
                    if let Some(s) = many.iter().find(|s| s.start == ws && s.end == we) {
                        return Ok((s.start, s.end));
                    }
                }
                Err(ApplyError::Ambiguous {
                    symbol: symbol.to_string(),
                    ranges: many.iter().map(|s| (s.start, s.end)).collect(),
                })
            }
        };
    }

    let matches: Vec<&vyer_incr::Symbol> =
        syms.symbols.iter().filter(|s| s.name == symbol).collect();
    match matches.as_slice() {
        [] => Err(ApplyError::SymbolNotFound {
            symbol: symbol.to_string(),
        }),
        [only] => Ok((only.start, only.end)),
        many => {
            if let Some((ws, we)) = want_range {
                if let Some(s) = many.iter().find(|s| s.start == ws && s.end == we) {
                    return Ok((s.start, s.end));
                }
            }
            Err(ApplyError::Ambiguous {
                symbol: symbol.to_string(),
                ranges: many.iter().map(|s| (s.start, s.end)).collect(),
            })
        }
    }
}

/// Return a symbol's exact source text and its 1-based inclusive span (SP-5 move).
pub fn symbol_text(
    current: &str,
    syms: &SymbolTable,
    symbol: &str,
    want_range: Option<(u32, u32)>,
) -> Result<(String, (u32, u32)), ApplyError> {
    let (start, end) = resolve_span(syms, symbol, want_range)?;
    let lines: Vec<&str> = current.lines().collect();
    let s = (start as usize).saturating_sub(1);
    let e = (end as usize).min(lines.len());
    if s >= e {
        return Err(ApplyError::SymbolNotFound {
            symbol: symbol.to_string(),
        });
    }
    Ok((lines[s..e].join("\n"), (start, end)))
}

/// Delete a symbol's node (SCRY-026), swallowing one trailing blank line so the
/// file stays tidy. Re-parse validated like every edit.
/// SCRY-075: walk backward from a symbol's first line over the doc-comments,
/// attributes, and decorators that BELONG to it, returning the index where they
/// begin. A delete/move must take these along — orphaning a `#[inline]`/`#[test]`
/// onto the next item is a compile error (it would re-decorate the wrong symbol),
/// and a stranded `/// doc` or `@decorator` is wrong too. Conservative: only the
/// unambiguously-attached OUTER forms (Rust `#[…]`, `///` outer docs, `@`
/// decorators in decorator languages); never an INNER module-level `#![…]`/`//!`,
/// never a plain `//`/`#` comment (ambiguous), and it stops at a blank line.
pub fn leading_trivia_start(lines: &[&str], s_idx: usize, lang: Lang) -> usize {
    let decorator = matches!(
        lang,
        Lang::Python
            | Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Dart
            | Lang::Java
            | Lang::Kotlin
    );
    let rust_attr = matches!(lang, Lang::Rust);
    let mut start = s_idx;
    while start > 0 {
        let t = lines[start - 1].trim();
        // single-line attached trivia
        if (rust_attr && t.starts_with("#["))
            || t.starts_with("///")
            || (decorator && t.starts_with('@'))
        {
            start -= 1;
            continue;
        }
        // a `/** … */` DOC block (JSDoc / Rust outer block doc) ending just above.
        // Only `/**`-opened blocks are consumed — never a plain `/*` license/code
        // block or an inner `/*!`, and only when the open/close are balanced.
        if t.ends_with("*/") {
            let mut k = start - 1;
            let open = loop {
                let lt = lines[k].trim();
                if lt.starts_with("/*") {
                    break Some((k, lt.starts_with("/**")));
                }
                if k == 0 {
                    break None;
                }
                k -= 1;
            };
            if let Some((open_idx, true)) = open {
                start = open_idx;
                continue;
            }
        }
        break;
    }
    start
}

pub fn prepare_delete(
    path: &str,
    current: &str,
    syms: &SymbolTable,
    lang: Lang,
    symbol: &str,
    want_range: Option<(u32, u32)>,
) -> Result<PreparedEdit, ApplyError> {
    let (start, end) = resolve_span(syms, symbol, want_range)?;
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    // SCRY-075: extend the deletion up over the symbol's own attributes/docs.
    let s_idx = leading_trivia_start(&lines, (start as usize).saturating_sub(1), lang);
    let (s_idx, e_idx) = (s_idx, end as usize);
    if s_idx >= lines.len() || e_idx > lines.len() || s_idx >= e_idx {
        return Err(ApplyError::SymbolNotFound {
            symbol: symbol.to_string(),
        });
    }
    let mut drop_end = e_idx;
    if drop_end < lines.len() && lines[drop_end].trim().is_empty() {
        drop_end += 1; // remove one trailing blank separator line
    }
    let mut new_text = String::with_capacity(current.len());
    for l in &lines[..s_idx] {
        new_text.push_str(l);
    }
    for l in &lines[drop_end..] {
        new_text.push_str(l);
    }
    validate_reparse(&new_text, lang)?;
    let diff = line_diff(path, current, &new_text);
    Ok(PreparedEdit {
        new_text,
        diff,
        start,
        end,
    })
}

/// Splice `new_body` over the symbol's line span and validate the result. This
/// is the deterministic, AST-anchored edit. `new_body` replaces the *entire*
/// node text (lines `start..=end`).
pub fn prepare_deterministic(
    path: &str,
    current: &str,
    syms: &SymbolTable,
    lang: Lang,
    symbol: &str,
    want_range: Option<(u32, u32)>,
    new_body: &str,
) -> Result<PreparedEdit, ApplyError> {
    let (start, end) = resolve_span(syms, symbol, want_range)?;
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    // Guard against an out-of-range span (a stale locator); resolve_span gives
    // spans straight from the current symbol table, so this is belt-and-braces.
    let (s_idx, e_idx) = ((start as usize).saturating_sub(1), end as usize);
    if s_idx >= lines.len() || e_idx > lines.len() || s_idx >= e_idx {
        return Err(ApplyError::SymbolNotFound {
            symbol: symbol.to_string(),
        });
    }

    let mut new_text = String::with_capacity(current.len() + new_body.len());
    for l in &lines[..s_idx] {
        new_text.push_str(l);
    }
    new_text.push_str(new_body);
    if !new_body.ends_with('\n') {
        new_text.push('\n');
    }
    for l in &lines[e_idx..] {
        new_text.push_str(l);
    }

    validate_reparse(&new_text, lang)?;

    let old_lines: Vec<&str> = current.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let new_body_len = new_body.lines().count().max(1);
    let diff = unified_diff(path, &old_lines, &new_lines, start, end, new_body_len);

    Ok(PreparedEdit {
        new_text,
        diff,
        start,
        end,
    })
}

/// Anchored sub-symbol edit (SCRY-004): replace the unique occurrence of
/// `anchor` with `replace`, searching only within `symbol`'s span (or the whole
/// file when `symbol` is None — that is how module-level lines, imports, and
/// constants become editable, closing SCRY-002's edit side). Far fewer tokens
/// than a full-body resend, and structurally unable to corrupt other lines.
#[allow(clippy::too_many_arguments)]
pub fn prepare_anchored(
    path: &str,
    current: &str,
    syms: &SymbolTable,
    lang: Lang,
    symbol: Option<&str>,
    want_range: Option<(u32, u32)>,
    anchor: &str,
    replace: &str,
) -> Result<PreparedEdit, ApplyError> {
    if anchor.is_empty() {
        return Err(ApplyError::AnchorNotFound {
            anchor: String::new(),
        });
    }
    // Restrict the search to the target scope's 1-based inclusive line range.
    let (lo, hi) = match symbol {
        Some(sym) => resolve_span(syms, sym, want_range)?,
        None => (1, current.lines().count().max(1) as u32),
    };
    // Collect byte offsets of every `anchor` occurrence whose start line is in scope.
    let mut hits: Vec<usize> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = current[from..].find(anchor) {
        let pos = from + rel;
        let line = current[..pos].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
        if line >= lo && line <= hi {
            hits.push(pos);
        }
        from = pos + anchor.len();
    }
    let pos = match hits.as_slice() {
        [] => {
            // SCRY-006: distinguish "truly absent" from "present but
            // whitespace-different" (the #1 cause of a failed anchored edit). If a
            // whitespace-normalized form of the anchor occurs in the scope text,
            // tell the agent it's a whitespace issue rather than a bare miss.
            let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
            let scope_text: String = current
                .lines()
                .skip((lo.saturating_sub(1)) as usize)
                .take((hi.saturating_sub(lo) + 1) as usize)
                .collect::<Vec<_>>()
                .join("\n");
            let want = normalize(anchor);
            if !want.is_empty() && normalize(&scope_text).contains(&want) {
                return Err(ApplyError::AnchorWhitespaceMismatch {
                    anchor: anchor.to_string(),
                });
            }
            return Err(ApplyError::AnchorNotFound {
                anchor: anchor.to_string(),
            });
        }
        [p] => *p,
        many => {
            return Err(ApplyError::AnchorAmbiguous {
                anchor: anchor.to_string(),
                count: many.len(),
            })
        }
    };

    let mut new_text = String::with_capacity(current.len() + replace.len());
    new_text.push_str(&current[..pos]);
    new_text.push_str(replace);
    new_text.push_str(&current[pos + anchor.len()..]);

    validate_reparse(&new_text, lang)?;
    let diff = line_diff(path, current, &new_text);
    let start = current[..pos].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    Ok(PreparedEdit {
        new_text,
        diff,
        start,
        end: start,
    })
}

/// Insert a freshly-authored `new_body` at `pos` (SCRY-013) — the thing the
/// replace-only path could never do (add a new function/method/test). Validated
/// by re-parse like every other edit.
/// The leading whitespace of a line (spaces/tabs).
fn line_indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// SCRY-077: re-base a block to `indent` — strip its own MINIMUM indentation (so
/// internal relative structure is preserved) and prefix every non-blank line with
/// `indent`. Idempotent: a block already at `indent` is unchanged. Makes the
/// insert ops forgiving about the caller's whitespace and keeps Python correct.
fn reindent_block(body: &str, indent: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let base = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{}", &l[base..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn prepare_insert(
    path: &str,
    current: &str,
    syms: &SymbolTable,
    lang: Lang,
    pos: InsertPos,
    new_body: &str,
) -> Result<PreparedEdit, ApplyError> {
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    let at: usize = match &pos {
        InsertPos::End => lines.len(),
        InsertPos::After(sym, r) => resolve_span(syms, sym, *r)?.1 as usize, // after end line
        InsertPos::Before(sym, r) => {
            // SCRY-076: insert before the symbol's OWN attributes/docs, not between
            // them and the symbol — that would split a `#[inline]`/`/// doc` off
            // onto the inserted item (the SCRY-075 hazard, for `@before`).
            let start = resolve_span(syms, sym, *r)?.0 as usize;
            leading_trivia_start(&lines, start.saturating_sub(1), lang)
        }
        InsertPos::Into(sym, r) => {
            let (lo, hi) = resolve_span(syms, sym, *r)?;
            // A single-line block has no interior to splice into — refuse rather
            // than misplace the member.
            if hi <= lo {
                return Err(ApplyError::ContainerInsertUnsupported {
                    container: sym.clone(),
                });
            }
            // Delimiter-closed languages (`}` or Ruby's `end`) get the member just
            // before the closing line; indentation-closed Python appends it after
            // the last body line (its indentation keeps it inside the block). The
            // real tree-sitter parse gate rejects any splice that doesn't parse.
            let before_close = matches!(
                lang,
                Lang::Rust
                    | Lang::Go
                    | Lang::Java
                    | Lang::Kotlin
                    | Lang::Swift
                    | Lang::C
                    | Lang::Cpp
                    | Lang::CSharp
                    | Lang::JavaScript
                    | Lang::TypeScript
                    | Lang::Tsx
                    | Lang::Dart
                    | Lang::Php
                    | Lang::Ruby
            );
            match lang {
                _ if before_close => (hi as usize).saturating_sub(1),
                Lang::Python => hi as usize,
                _ => {
                    return Err(ApplyError::ContainerInsertUnsupported {
                        container: sym.clone(),
                    })
                }
            }
        }
    };
    let at = at.min(lines.len());

    // SCRY-077: auto-indent the inserted text to match its surroundings, so the
    // caller needn't compute leading whitespace — and Python (where indentation is
    // SEMANTIC) stays correct. Idempotent: text already at the right indent is
    // unchanged. The indent is the anchor symbol's (for `@after`/`@before`) or an
    // existing member's (for `@into`); `@end` stays at column 0.
    let indent: String = match &pos {
        InsertPos::End => String::new(),
        InsertPos::After(sym, r) | InsertPos::Before(sym, r) => {
            let s0 = (resolve_span(syms, sym, *r)?.0 as usize).saturating_sub(1);
            line_indent(lines[s0.min(lines.len().saturating_sub(1))]).to_string()
        }
        InsertPos::Into(_, _) => {
            // an existing member's indent (the non-blank line just inside the
            // insertion point); column 0 only for a truly empty container.
            (0..at)
                .rev()
                .map(|i| lines[i])
                .find(|l| !l.trim().is_empty())
                .map(|l| line_indent(l).to_string())
                .unwrap_or_default()
        }
    };
    let reindented = reindent_block(new_body, &indent);

    let mut block = String::from("\n");
    block.push_str(reindented.trim_end_matches('\n'));
    block.push('\n');

    let mut new_text = String::with_capacity(current.len() + block.len());
    for l in &lines[..at] {
        new_text.push_str(l);
    }
    new_text.push_str(&block);
    for l in &lines[at..] {
        new_text.push_str(l);
    }

    validate_reparse(&new_text, lang)?;
    let diff = line_diff(path, current, &new_text);
    let start = at as u32 + 1;
    Ok(PreparedEdit {
        new_text,
        diff,
        start,
        end: start,
    })
}

/// A general line-level unified diff (common-prefix/suffix collapse), used by the
/// anchored and insert paths where the changed region isn't a single symbol span.
pub fn line_diff(path: &str, old_text: &str, new_text: &str) -> String {
    let old: Vec<&str> = old_text.lines().collect();
    let new: Vec<&str> = new_text.lines().collect();
    let mut p = 0usize;
    while p < old.len() && p < new.len() && old[p] == new[p] {
        p += 1;
    }
    let mut s = 0usize;
    while s < old.len().saturating_sub(p)
        && s < new.len().saturating_sub(p)
        && old[old.len() - 1 - s] == new[new.len() - 1 - s]
    {
        s += 1;
    }
    const CTX: usize = 3;
    let cb = p.min(CTX);
    let old_chg_end = old.len() - s;
    let new_chg_end = new.len() - s;
    let ca = s.min(CTX);
    let old_count = (old_chg_end - p) + cb + ca;
    let new_count = (new_chg_end - p) + cb + ca;
    // SCRY-095: sanitize the path against envelope injection (a pathological
    // filename the agent happens to edit) — same class as the result envelope.
    let sp = sanitize_field(path);
    let mut out = format!("--- a/{sp}\n+++ b/{sp}\n");
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        p - cb + 1,
        old_count,
        p - cb + 1,
        new_count
    ));
    for line in &old[p - cb..p] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old[p..old_chg_end] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new[p..new_chg_end] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old[old_chg_end..old_chg_end + ca] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// The re-parse gate. With the heuristic core this is a structural sanity check:
/// the whole file must have balanced `()[]{}` outside strings/char/line-comments
/// for the supported brace languages. When tree-sitter swaps in at the same
/// `parse` call site (Phase 2), this becomes a true "does the AST still parse?"
/// check with zero changes to callers.
pub fn validate_reparse(text: &str, lang: Lang) -> Result<(), ApplyError> {
    // The engine runs the real tree-sitter `has_parse_error` gate after every
    // edit (SCRY-001); this cheap pre-check just rejects obvious bracket
    // imbalance early. The key subtlety is how `'` is treated:
    //   - Rust/Go/Java/Kotlin/Swift: `'` is a char literal OR (Rust) a lifetime
    //     (`'static`) — NOT a string. Treating it as a string desynced the scan
    //     on every real Rust file (the bug that blocked editing vyer's own source).
    //   - JS/TS/Dart: `'` is a string delimiter.
    //   - Python/Ruby: block structure isn't brace-delimited → no brace gate.
    match lang {
        Lang::Python | Lang::Generic | Lang::Ruby => Ok(()),
        Lang::Rust
        | Lang::Go
        | Lang::Java
        | Lang::Kotlin
        | Lang::Swift
        | Lang::C
        | Lang::Cpp
        | Lang::CSharp => brace_balanced(text, false),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Dart | Lang::Php => {
            brace_balanced(text, true)
        }
    }
}

/// Balance `()[]{}` outside strings/char-literals/line-comments. `sq_is_string`
/// selects single-quote handling: a string delimiter (JS/TS/Dart) vs a char
/// literal/lifetime (Rust/Go/Java/Kotlin/Swift). Index-based so we can look ahead
/// to tell `'x'` (char) from `'static` (lifetime).
fn brace_balanced(text: &str, sq_is_string: bool) -> Result<(), ApplyError> {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut stack: Vec<char> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let c = cs[i];
        // line comment
        if c == '/' && i + 1 < n && cs[i + 1] == '/' {
            while i < n && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // double-quoted string
        if c == '"' {
            i += 1;
            while i < n {
                if cs[i] == '\\' {
                    i += 2;
                    continue;
                }
                if cs[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // single quote
        if c == '\'' {
            if sq_is_string {
                i += 1;
                while i < n {
                    if cs[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if cs[i] == '\'' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            } else if i + 1 < n && cs[i + 1] == '\\' {
                // escape char literal '\n' / '\'' — skip to the closing quote
                i += 3; // ' \ x
                if i < n && cs[i] == '\'' {
                    i += 1;
                }
            } else if i + 2 < n && cs[i + 2] == '\'' {
                i += 3; // 'x'
            } else {
                i += 1; // a lifetime/label ('static, 'a): the ' is not a delimiter
            }
            continue;
        }
        match c {
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let want = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                match stack.pop() {
                    Some(open) if open == want => {}
                    _ => {
                        return Err(ApplyError::ReparseFailed {
                            reason: format!("unbalanced `{c}`"),
                        })
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(open) = stack.last() {
        return Err(ApplyError::ReparseFailed {
            reason: format!("unclosed `{open}`"),
        });
    }
    Ok(())
}

/// A minimal, deterministic unified diff. We *know* the replaced region exactly
/// (lines `start..=end` became `new_len` lines), so we emit a single hunk with
/// up to three lines of surrounding context — no LCS needed, fully reproducible.
fn unified_diff(
    path: &str,
    old: &[&str],
    new: &[&str],
    start: u32,
    end: u32,
    new_len: usize,
) -> String {
    const CTX: usize = 3;
    let s = start as usize; // 1-based
    let e = end as usize;
    let ctx_before = s.saturating_sub(1).min(CTX);
    let hunk_old_start = s - ctx_before; // 1-based
    let old_after_start = e; // 0-based index of first line after region
    let ctx_after = (old.len().saturating_sub(old_after_start)).min(CTX);

    let old_count = (e - s + 1) + ctx_before + ctx_after;
    let new_count = new_len + ctx_before + ctx_after;

    let mut out = String::new();
    let sp = sanitize_field(path);
    out.push_str(&format!("--- a/{sp}\n+++ b/{sp}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk_old_start, old_count, hunk_old_start, new_count
    ));
    // leading context
    for line in &old[(s - 1 - ctx_before)..(s - 1)] {
        out.push_str(&format!(" {line}\n"));
    }
    // removed
    for line in &old[(s - 1)..e] {
        out.push_str(&format!("-{line}\n"));
    }
    // added: the new lines occupy the same hunk position (after the shared
    // leading context lines).
    for line in new.iter().skip(s - 1).take(new_len) {
        out.push_str(&format!("+{line}\n"));
    }
    // trailing context
    for line in &old[old_after_start..(old_after_start + ctx_after)] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyer_incr::Db;

    fn syms(path: &str, text: &str) -> (SymbolTable, Lang) {
        let mut db = Db::new();
        db.set_text(path, text);
        let st = db.symbols(path);
        // clone out of the Arc for the pure functions
        (
            SymbolTable {
                symbols: st.symbols.clone(),
            },
            if path.ends_with(".py") {
                Lang::Python
            } else {
                Lang::Rust
            },
        )
    }

    #[test]
    fn leading_trivia_start_carries_docs_but_not_license_or_inner() {
        // SCRY-075: extend a symbol's span up over its OWN outer attrs/docs.
        let rust = [
            "//! module doc\n", // 0 inner module doc — must NOT be consumed
            "/// item doc\n",   // 1 outer doc
            "#[inline]\n",      // 2 attribute
            "fn foo() {}\n",    // 3 the symbol
        ];
        // symbol at idx 3 → trivia starts at idx 1 (doc+attr), NOT idx 0 (module doc).
        assert_eq!(leading_trivia_start(&rust, 3, Lang::Rust), 1);

        // a `/**` doc block is consumed; a plain `/*` license block is NOT.
        let jsdoc = ["/**\n", " * docs\n", " */\n", "function add() {}\n"];
        assert_eq!(leading_trivia_start(&jsdoc, 3, Lang::JavaScript), 0);
        let license = ["/* license\n", "   header */\n", "fn first() {}\n"];
        assert_eq!(
            leading_trivia_start(&license, 2, Lang::Rust),
            2,
            "a plain /* license block must NOT be consumed"
        );

        // a `@` decorator is consumed only in decorator languages, not Rust.
        let py = ["@decorator\n", "def foo():\n"];
        assert_eq!(leading_trivia_start(&py, 1, Lang::Python), 0);
        let rust_at = ["@something\n", "fn foo() {}\n"];
        assert_eq!(leading_trivia_start(&rust_at, 1, Lang::Rust), 1);
    }

    #[test]
    fn reindent_block_rebases_preserving_relative_structure() {
        // SCRY-077: shift a block to `indent`, preserving INTERNAL relative indent.
        assert_eq!(
            reindent_block("fn b() {\n    x;\n}", "    "),
            "    fn b() {\n        x;\n    }"
        );
        // idempotent: a block already at the target indent is unchanged.
        assert_eq!(reindent_block("    a: u8,", "    "), "    a: u8,");
        // single unindented line gets the indent.
        assert_eq!(reindent_block("a: u8,", "    "), "    a: u8,");
        // blank lines stay blank (no trailing-whitespace indent).
        assert_eq!(reindent_block("a\n\nb", "  "), "  a\n\n  b");
    }

    const FILE: &str = "pub fn a() -> i32 {\n    1\n}\npub fn b() -> i32 {\n    2\n}\n";

    #[test]
    fn deterministic_splice_replaces_only_target_symbol() {
        let (st, lang) = syms("m.rs", FILE);
        let edit = prepare_deterministic(
            "m.rs",
            FILE,
            &st,
            lang,
            "b",
            None,
            "pub fn b() -> i32 {\n    42\n}",
        )
        .unwrap();
        assert!(edit.new_text.contains("42"));
        assert!(edit.new_text.contains("pub fn a()")); // a() untouched
        assert!(edit.new_text.starts_with("pub fn a() -> i32 {\n    1\n}\n"));
        assert_eq!((edit.start, edit.end), (4, 6));
        assert!(edit.diff.contains("@@"));
        assert!(edit.diff.contains("+    42"));
        assert!(edit.diff.contains("-    2"));
    }

    #[test]
    fn rejects_edit_that_breaks_parse() {
        let (st, lang) = syms("m.rs", FILE);
        // missing closing brace
        let err = prepare_deterministic(
            "m.rs",
            FILE,
            &st,
            lang,
            "b",
            None,
            "pub fn b() -> i32 {\n    42\n",
        )
        .unwrap_err();
        assert!(
            matches!(err, ApplyError::ReparseFailed { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn unknown_symbol_is_rejected() {
        let (st, lang) = syms("m.rs", FILE);
        let err = prepare_deterministic("m.rs", FILE, &st, lang, "nope", None, "x").unwrap_err();
        assert_eq!(
            err,
            ApplyError::SymbolNotFound {
                symbol: "nope".into()
            }
        );
    }

    #[test]
    fn anchored_edit_replaces_unique_text_in_symbol_scope() {
        let (st, lang) = syms("m.rs", FILE);
        let edit =
            prepare_anchored("m.rs", FILE, &st, lang, Some("b"), None, "    2", "    99").unwrap();
        assert!(edit.new_text.contains("99"));
        assert!(edit.new_text.contains("    1\n"), "a() body untouched");
        assert!(edit.diff.contains("+    99") && edit.diff.contains("-    2"));
    }

    #[test]
    fn anchored_edit_ambiguous_is_rejected() {
        let text = "pub fn a() -> i32 {\n    let x = 1;\n    let x = 1;\n    0\n}\n";
        let (st, lang) = syms("m.rs", text);
        let err = prepare_anchored(
            "m.rs",
            text,
            &st,
            lang,
            Some("a"),
            None,
            "let x = 1;",
            "let x = 2;",
        )
        .unwrap_err();
        assert!(
            matches!(err, ApplyError::AnchorAmbiguous { count: 2, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn anchored_edit_file_scope_edits_module_level() {
        // No symbol → whole-file scope: this is how imports/constants get edited.
        let text = "use foo;\npub fn a() {}\n";
        let (st, lang) = syms("m.rs", text);
        let edit = prepare_anchored(
            "m.rs",
            text,
            &st,
            lang,
            None,
            None,
            "use foo;",
            "use foo;\nuse bar;",
        )
        .unwrap();
        assert!(edit.new_text.contains("use bar;"));
        assert!(edit.new_text.contains("pub fn a()"));
    }

    #[test]
    fn anchored_whitespace_mismatch_is_diagnosed() {
        let src = "fn f() {\n    let x = 1;\n}\n";
        let (st, lang) = syms("a.rs", src);
        // Same tokens, different internal spacing — not a literal substring, but a
        // whitespace-normalized match exists → diagnosed as a whitespace issue.
        let err = prepare_anchored(
            "a.rs",
            src,
            &st,
            lang,
            None,
            None,
            "let  x  =  1;",
            "let x = 2;",
        )
        .unwrap_err();
        assert!(
            matches!(err, ApplyError::AnchorWhitespaceMismatch { .. }),
            "expected whitespace diagnosis, got: {err}"
        );
        assert!(err.to_string().contains("whitespace"), "msg: {err}");

        // A genuinely absent anchor stays a plain not-found.
        let err2 = prepare_anchored("a.rs", src, &st, lang, None, None, "nonexistent_token", "x")
            .unwrap_err();
        assert!(
            matches!(err2, ApplyError::AnchorNotFound { .. }),
            "expected not-found, got: {err2}"
        );
    }

    #[test]
    fn insert_after_symbol_and_at_end() {
        let (st, lang) = syms("m.rs", FILE);
        let after = prepare_insert(
            "m.rs",
            FILE,
            &st,
            lang,
            InsertPos::After("a".into(), None),
            "fn c() {}",
        )
        .unwrap();
        assert!(after.new_text.contains("fn c()"));
        assert!(after.new_text.contains("pub fn a()") && after.new_text.contains("pub fn b()"));
        // c() lands between a() and b().
        let ia = after.new_text.find("fn a").unwrap();
        let ic = after.new_text.find("fn c").unwrap();
        let ib = after.new_text.find("fn b").unwrap();
        assert!(
            ia < ic && ic < ib,
            "insert-after ordering: {}",
            after.new_text
        );

        let end = prepare_insert("m.rs", FILE, &st, lang, InsertPos::End, "fn z() {}").unwrap();
        assert!(end.new_text.trim_end().ends_with("fn z() {}"));
    }

    #[test]
    fn insert_into_container_before_closing_brace() {
        // Brace language: a field is spliced in just before the struct's `}`.
        let src = "struct S {\n    x: u8,\n}\n";
        let (st, lang) = syms("m.rs", src);
        let edit = prepare_insert(
            "m.rs",
            src,
            &st,
            lang,
            InsertPos::Into("S".into(), None),
            "    y: u8,",
        )
        .unwrap();
        assert!(edit.new_text.contains("x: u8,") && edit.new_text.contains("y: u8,"));
        let yi = edit.new_text.find("y: u8").unwrap();
        let last_brace = edit.new_text.rfind('}').unwrap();
        assert!(
            yi < last_brace,
            "member inserted outside the container: {}",
            edit.new_text
        );
        assert!(
            edit.new_text.find("x: u8").unwrap() < yi,
            "existing member should precede the new one: {}",
            edit.new_text
        );

        // Python: the member is appended to the class body (indentation keeps it
        // inside the block) — now supported, not refused.
        let py = "class C:\n    a = 1\n    b = 2\n";
        let (pst, plang) = syms("m.py", py);
        let pedit = prepare_insert(
            "m.py",
            py,
            &pst,
            plang,
            InsertPos::Into("C".into(), None),
            "    c = 3",
        )
        .unwrap();
        assert!(
            pedit.new_text.contains("c = 3"),
            "python member not inserted: {}",
            pedit.new_text
        );
        assert!(
            pedit.new_text.find("b = 2").unwrap() < pedit.new_text.find("c = 3").unwrap(),
            "python member out of order: {}",
            pedit.new_text
        );

        // A single-line block has no interior → refused (never misplaced).
        let one = "struct One { x: u8 }\n";
        let (ost, olang) = syms("o.rs", one);
        let err = prepare_insert(
            "o.rs",
            one,
            &ost,
            olang,
            InsertPos::Into("One".into(), None),
            "    y: u8,",
        )
        .unwrap_err();
        assert!(
            matches!(err, ApplyError::ContainerInsertUnsupported { .. }),
            "single-line @into should be refused, got: {err}"
        );
    }

    #[test]
    fn brace_balance_ignores_braces_in_strings_and_comments() {
        assert!(brace_balanced("fn f() { let s = \"}\"; // }\n }\n", false).is_ok());
        assert!(brace_balanced("fn f() { ", false).is_err());
        // Rust lifetimes & char-literals must NOT desync the scan (the bug that
        // blocked editing vyer's own source).
        assert!(brace_balanced("fn f<'a>(x: &'a str) -> &'static str { \"ok\" }\n", false).is_ok());
        assert!(brace_balanced("fn g() { let c = '}'; let d = '{'; }\n", false).is_ok());
    }
}
