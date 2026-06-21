//! Lexical search over in-memory file text, using ripgrep's own libraries
//! (`grep-regex` + `grep-searcher`) — never by shelling out to the `rg` binary.
//!
//! We search the text the incremental core currently holds (not the raw disk),
//! so a read right after a write reflects the edit (read-after-write freshness):
//! the apply path updates the in-memory input synchronously, and search reads
//! that same input. Disk and the warm core can never disagree mid-query.

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;

/// One matching line within a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    pub line: u32, // 1-based
    pub text: String,
}

/// Build a matcher for `pattern`. We treat the query as a regex (so structural
/// callers can pass anchored patterns), but smart-case it (case-insensitive
/// unless the pattern itself contains an uppercase letter) and fall back to a
/// literal, regex-escaped search when the pattern is not a valid regex. Returns
/// `None` only if even the escaped literal fails to compile (never expected).
fn build_matcher(pattern: &str) -> Option<RegexMatcher> {
    let smart_case = !pattern.chars().any(|c| c.is_uppercase());
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(smart_case);
    if let Ok(m) = b.build(pattern) {
        return Some(m);
    }
    b.build(&regex_escape(pattern)).ok()
}

/// Escape regex metacharacters so an arbitrary identifier is matched literally.
fn regex_escape(s: &str) -> String {
    const META: &[char] = &[
        '\\', '.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$', '#', '-',
    ];
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Search a single file's `text` for `pattern`, returning up to `max_hits`
/// matching lines. Deterministic: lines are emitted in file order.
pub fn search_text(text: &str, pattern: &str, max_hits: usize) -> Vec<LineHit> {
    let matcher = match build_matcher(pattern) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut hits = Vec::new();
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    // search_slice never errors for an in-memory UTF-8 slice with our sink, but
    // we deliberately ignore the Result rather than unwrap (no panics in the
    // query path — a malformed input degrades to "no hits", never a crash).
    let _ = searcher.search_slice(
        &matcher,
        text.as_bytes(),
        UTF8(|lnum, line| {
            if hits.len() < max_hits {
                hits.push(LineHit {
                    line: lnum as u32,
                    text: line.trim_end_matches(['\n', '\r']).to_string(),
                });
            }
            // Returning Ok(false) stops the search early once we are full.
            Ok(hits.len() < max_hits)
        }),
    );
    hits
}

/// Boolean line-level search: keep lines that match the optional `base` regex
/// AND contain every `all_of` literal AND at least one `any_of` literal (when
/// non-empty) AND none of the `none_of` literals. One Aho-Corasick pass detects
/// all operands at once (O(n+matches) regardless of operand count — vs one grep
/// per term), and the per-line predicate is three bitmask ops. Up to 64 operands
/// (extras beyond 64 are ignored — the caller advertises the cap).
///
/// `line_starts` is the line-offset index; matches are mapped to their line via
/// binary search. Deterministic: lines are emitted in file order.
pub fn search_bool(
    text: &str,
    base: &str,
    all_of: &[String],
    any_of: &[String],
    none_of: &[String],
    line_starts: &[u32],
    max_hits: usize,
) -> Vec<LineHit> {
    use aho_corasick::AhoCorasick;
    use grep_matcher::Matcher;

    // Assign each operand a bit; build the three masks. Cap at 64 (u64 presence).
    let mut terms: Vec<&str> = Vec::new();
    let (mut all_mask, mut any_mask, mut none_mask) = (0u64, 0u64, 0u64);
    for s in all_of {
        if !s.is_empty() && terms.len() < 64 {
            all_mask |= 1u64 << terms.len();
            terms.push(s);
        }
    }
    for s in any_of {
        if !s.is_empty() && terms.len() < 64 {
            any_mask |= 1u64 << terms.len();
            terms.push(s);
        }
    }
    for s in none_of {
        if !s.is_empty() && terms.len() < 64 {
            none_mask |= 1u64 << terms.len();
            terms.push(s);
        }
    }

    // Smart-case across operands: case-insensitive unless some operand has an
    // uppercase letter (mirrors `build_matcher`).
    let ci = !terms.iter().any(|t| t.chars().any(|c| c.is_uppercase()));
    let ac = if terms.is_empty() {
        None
    } else {
        AhoCorasick::builder()
            .ascii_case_insensitive(ci)
            .build(&terms)
            .ok()
    };

    // Per-line presence bitset. `find_overlapping_iter` (MatchKind::Standard)
    // reports every operand occurrence even when operands overlap, so presence
    // detection is exact (a plain leftmost scan could hide a nested operand).
    let mut present = vec![0u64; line_starts.len()];
    if let Some(ac) = &ac {
        for m in ac.find_overlapping_iter(text) {
            let ln = line_of(line_starts, m.start() as u32);
            present[ln] |= 1u64 << m.pattern().as_usize();
        }
    }

    let base_matcher = if base.is_empty() {
        None
    } else {
        build_matcher(base)
    };
    let mut hits = Vec::new();
    for (i, &start) in line_starts.iter().enumerate() {
        let p = present[i];
        let ok = (p & all_mask) == all_mask
            && (any_mask == 0 || p & any_mask != 0)
            && (p & none_mask) == 0;
        if !ok {
            continue;
        }
        let s = start as usize;
        let e = line_starts
            .get(i + 1)
            .map(|o| *o as usize)
            .unwrap_or(text.len());
        let line = &text[s..e];
        if let Some(m) = &base_matcher {
            if !m.is_match(line.as_bytes()).unwrap_or(false) {
                continue;
            }
        }
        hits.push(LineHit {
            line: (i + 1) as u32,
            text: line.trim_end_matches(['\n', '\r']).to_string(),
        });
        if hits.len() >= max_hits {
            break;
        }
    }
    hits
}

/// The line number (0-based) containing byte offset `off`, via binary search on
/// the line-offset index (`line_starts[i]` = byte offset of line `i`'s start).
fn line_of(line_starts: &[u32], off: u32) -> usize {
    match line_starts.binary_search(&off) {
        Ok(i) => i,                    // off is exactly a line start
        Err(i) => i.saturating_sub(1), // off falls inside the previous line
    }
}

/// Count occurrences of a **literal** `needle` (case-sensitive, like `grep -c`)
/// and how many distinct lines contain at least one — in a single SIMD pass via
/// `memchr::memmem`, allocating nothing. `line_starts` is the line-offset index.
/// Returns `(lines_with_match, total_matches)`.
///
/// Distinct-line counting is exact because `memmem::find_iter` yields matches in
/// strictly increasing offset order, so a line's matches are contiguous and a
/// `last_line` watermark never double-counts or misses.
pub fn count_literal(text: &str, needle: &str, line_starts: &[u32]) -> (usize, usize) {
    if needle.is_empty() {
        return (0, 0);
    }
    let mut matches = 0usize;
    let mut lines = 0usize;
    let mut last_line = usize::MAX;
    for pos in memchr::memmem::find_iter(text.as_bytes(), needle.as_bytes()) {
        matches += 1;
        let ln = line_of(line_starts, pos as u32);
        if ln != last_line {
            lines += 1;
            last_line = ln;
        }
    }
    (lines, matches)
}

/// Count matches (and matching lines) for a **regex** pattern, smart-cased like
/// `search_text`, for queries that are not plain identifiers. Non-overlapping,
/// leftmost (grep semantics); a zero-width match advances by one to terminate.
pub fn count_regex(text: &str, pattern: &str, line_starts: &[u32]) -> (usize, usize) {
    use grep_matcher::Matcher;
    let matcher = match build_matcher(pattern) {
        Some(m) => m,
        None => return (0, 0),
    };
    let bytes = text.as_bytes();
    let mut matches = 0usize;
    let mut lines = 0usize;
    let mut last_line = usize::MAX;
    let mut at = 0usize;
    while at <= bytes.len() {
        match matcher.find_at(bytes, at) {
            Ok(Some(m)) => {
                matches += 1;
                let ln = line_of(line_starts, m.start() as u32);
                if ln != last_line {
                    lines += 1;
                    last_line = ln;
                }
                at = if m.end() > m.start() {
                    m.end()
                } else {
                    m.end() + 1
                };
            }
            _ => break,
        }
    }
    (lines, matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str =
        "fn validate_token(t: &str) -> bool {\n    Token::parse(t).is_ok()\n}\nfn LOGIN() {}\n";

    #[test]
    fn finds_literal_identifier() {
        let hits = search_text(SRC, "validate_token", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
        assert!(hits[0].text.contains("validate_token"));
    }

    #[test]
    fn smart_case_is_case_insensitive_for_lowercase_query() {
        // lowercase query => case-insensitive => matches LOGIN
        assert_eq!(search_text(SRC, "login", 10).len(), 1);
        // uppercase present => case-sensitive => does NOT match `fn validate`
        assert_eq!(search_text(SRC, "LOGIN", 10).len(), 1);
        assert_eq!(search_text(SRC, "Validate_Token", 10).len(), 0);
    }

    #[test]
    fn regex_query_works_and_invalid_regex_degrades_to_literal() {
        assert_eq!(search_text(SRC, "fn .*token", 10).len(), 1);
        // an unbalanced bracket is an invalid regex => literal search => 0 hits
        assert_eq!(search_text("a[b literal", "a[b", 10).len(), 1);
    }

    #[test]
    fn respects_max_hits() {
        let many = "x\nx\nx\nx\nx\n";
        assert_eq!(search_text(many, "x", 2).len(), 2);
    }

    #[test]
    fn search_bool_none_of_excludes_lines_with_the_term() {
        let text = "fn alpha() { unwrap() }\nfn beta() { expect() }\nfn gamma() { plain() }\n";
        let ls = starts(text);
        // all_of: only the line with BOTH `fn` and `unwrap` (alpha).
        let all = search_bool(text, "fn", &["unwrap".into()], &[], &[], &ls, usize::MAX);
        let all_lines: Vec<u32> = all.iter().map(|h| h.line).collect();
        assert_eq!(all_lines, vec![1], "all_of: {all_lines:?}");
        // none_of: EXCLUDE the line containing `unwrap` (alpha) → beta + gamma.
        let none = search_bool(text, "fn", &[], &[], &["unwrap".into()], &ls, usize::MAX);
        let none_lines: Vec<u32> = none.iter().map(|h| h.line).collect();
        assert_eq!(
            none_lines,
            vec![2, 3],
            "none_of must drop the line with `unwrap`: {none_lines:?}"
        );
    }

    // line starts for a string, mirroring vyer_incr::Db::line_index, so the
    // count helpers can be tested in isolation.
    fn starts(text: &str) -> Vec<u32> {
        let b = text.as_bytes();
        let mut v = vec![0u32];
        for i in 0..b.len() {
            if b[i] == b'\n' && i + 1 < b.len() {
                v.push((i + 1) as u32);
            }
        }
        v
    }

    #[test]
    fn count_literal_reports_lines_and_matches() {
        let t = "foo bar foo\nbaz\nfoo\n"; // foo: 3 matches on 2 lines
        let (lines, matches) = count_literal(t, "foo", &starts(t));
        assert_eq!((lines, matches), (2, 3));
        // case-sensitive (grep -c default): FOO not matched
        assert_eq!(count_literal(t, "FOO", &starts(t)), (0, 0));
        // two matches on the SAME line count as one line, two matches
        let t2 = "abab\n";
        assert_eq!(count_literal(t2, "ab", &starts(t2)), (1, 2));
    }

    #[test]
    fn count_regex_smart_cased() {
        let t = "fn a()\nfn bb()\nFN c()\n";
        // smart-case (lowercase pattern) => case-insensitive => matches FN too
        let (lines, matches) = count_regex(t, "fn ", &starts(t));
        assert_eq!((lines, matches), (3, 3));
        // anchored regex
        assert_eq!(
            count_regex("x1\nx2\ny3\n", r"x\d", &starts("x1\nx2\ny3\n")),
            (2, 2)
        );
    }

    #[test]
    fn boolean_and_or_not() {
        let t = "alpha beta\nalpha\nbeta gamma\nalpha beta gamma\ndelta\n";
        let ls = starts(t);
        let s = |all: &[&str], any: &[&str], none: &[&str]| {
            let to = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            search_bool(t, "", &to(all), &to(any), &to(none), &ls, 100)
                .into_iter()
                .map(|h| h.line)
                .collect::<Vec<_>>()
        };
        // AND: lines with both alpha AND beta → lines 1 and 4
        assert_eq!(s(&["alpha", "beta"], &[], &[]), vec![1, 4]);
        // AND NOT: alpha AND beta but NOT gamma → line 1 only
        assert_eq!(s(&["alpha", "beta"], &[], &["gamma"]), vec![1]);
        // OR: any of gamma/delta → lines 3, 4, 5
        assert_eq!(s(&[], &["gamma", "delta"], &[]), vec![3, 4, 5]);
        // base regex AND boolean: lines matching `^alpha` AND containing gamma → line 4
        let to = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let got: Vec<u32> = search_bool(t, "^alpha", &to(&["gamma"]), &[], &[], &ls, 100)
            .into_iter()
            .map(|h| h.line)
            .collect();
        assert_eq!(got, vec![4]);
    }
}
