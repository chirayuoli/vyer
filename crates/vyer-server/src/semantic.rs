//! Semantic provider seam (LSP sidecar — Phase 1; see `docs/design-lsp-sidecar.md`).
//!
//! vyer's graph (`refs`/`impact`/`context`/blast-radius/safe-delete) is today a
//! lexical + tree-sitter APPROXIMATION. True type-resolved semantics need a
//! language server. This module is the seam that a real `LspProvider` (Phase 2+)
//! plugs into; the default [`NullProvider`] returns nothing, so the engine
//! transparently falls back to its approximation and reports the tier honestly
//! (Rule §8: degrade, don't crash; always disclose the tier so the agent calibrates).
//!
//! No external dependency is introduced here — only the trait + the honest-default
//! provider + tier reporting. The heavy rust-analyzer/tsserver integration is the
//! next phase, behind `--allow-lsp` and an operator allowlist (Rule §9).

/// How resolved the semantic answer is — surfaced to the agent so it can calibrate
/// trust (a `partial` ref list may miss type-resolved call sites; `none` means the
/// lexical/tree-sitter approximation is in use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Type-resolved by a language server (cross-file, scope-aware).
    Full,
    /// Lexical + tree-sitter approximation (vyer's built-in graph).
    Partial,
    /// No semantic resolution available for this language/target.
    None,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Full => "lsp",
            Tier::Partial => "lexical-approx",
            Tier::None => "none",
        }
    }
}

/// A type-resolved reference (Phase 2+ fills these). Kept minimal and transport-
/// agnostic so the engine maps it onto the existing span/locator output.
#[derive(Debug, Clone)]
pub struct SemRef {
    pub path: String,
    pub line: u32,
}

/// The seam every semantic backend implements. The engine consults it as an
/// UPGRADE: it computes its approximation first (so a `None`/timeout costs nothing),
/// then a `Some(..)` result replaces it and bumps the reported tier. Every method
/// is best-effort and MUST NOT panic or block indefinitely (Rule §8).
pub trait SemanticProvider: Send + Sync {
    /// Type-resolved references to the symbol named `name` defined in `def_file`.
    /// `None` → no semantic answer; caller falls back to the lexical approximation.
    fn references(&self, _def_file: &str, _name: &str) -> Option<Vec<SemRef>> {
        None
    }
    /// The semantic tier currently available (drives the honest `tier=` label).
    fn tier(&self) -> Tier {
        Tier::None
    }
    /// A short status string for `vyer://status` (e.g. "off" / "rust-analyzer").
    fn status(&self) -> String {
        "off (lexical/tree-sitter approximation)".to_string()
    }
}

/// The default: no language server. The engine's built-in approximation is used and
/// honestly reported as `tier=lexical-approx`. Selecting a real provider is Phase 2.
#[derive(Debug, Default)]
pub struct NullProvider;

impl SemanticProvider for NullProvider {}

/// LSP wire protocol (Phase 2a): the pure, fully-tested transport core a real
/// `LspProvider` (Phase 2b: spawn server + handshake) builds on. Kept here so the
/// hard correctness parts — Content-Length framing, JSON-RPC id correlation, the
/// `textDocument/references` request/response shapes — are unit-tested WITHOUT a
/// running language server (the happy path needs the server binary; this doesn't).
pub mod lsp {
    use serde_json::{json, Value};

    /// Frame a JSON-RPC body for the LSP base protocol: a `Content-Length` header,
    /// CRLFCRLF, then the body. This is exactly what an LSP server reads on stdin.
    pub fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    /// Parse ONE framed message from the front of `buf`. Returns the message body and
    /// the number of bytes consumed, or `None` if `buf` doesn't yet hold a full frame
    /// (the caller reads more bytes and retries — the standard streaming decode).
    pub fn parse_frame(buf: &[u8]) -> Option<(String, usize)> {
        const SEP: &[u8] = b"\r\n\r\n";
        let header_end = buf.windows(SEP.len()).position(|w| w == SEP)?;
        let header = std::str::from_utf8(&buf[..header_end]).ok()?;
        let len: usize = header.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim().eq_ignore_ascii_case("Content-Length")).then(|| v.trim().parse().ok())?
        })?;
        let body_start = header_end + SEP.len();
        let body_end = body_start.checked_add(len)?;
        if buf.len() < body_end {
            return None; // frame not fully buffered yet
        }
        let body = std::str::from_utf8(&buf[body_start..body_end])
            .ok()?
            .to_string();
        Some((body, body_end))
    }

    /// Build a `textDocument/references` JSON-RPC request (find all references to the
    /// symbol at `(line, character)`, 0-based per LSP) for the file `uri`.
    pub fn references_request(id: i64, uri: &str, line: u32, character: u32) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/references",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": false }
            }
        })
        .to_string()
    }

    /// Extract `(uri, line)` pairs from a `textDocument/references` RESULT (an array of
    /// LSP `Location`s). `None` if the body isn't a matching JSON-RPC response for `id`
    /// or carries an error — the caller then degrades to the approximation.
    pub fn parse_references(body: &str, id: i64) -> Option<Vec<(String, u32)>> {
        let v: Value = serde_json::from_str(body).ok()?;
        if v.get("id").and_then(Value::as_i64) != Some(id) || v.get("error").is_some() {
            return None;
        }
        let arr = v.get("result")?.as_array()?;
        let mut out = Vec::new();
        for loc in arr {
            let uri = loc.get("uri").and_then(Value::as_str)?.to_string();
            let line = loc
                .get("range")?
                .get("start")?
                .get("line")
                .and_then(Value::as_u64)? as u32;
            out.push((uri, line + 1)); // LSP is 0-based; vyer locators are 1-based
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn frame_roundtrips_and_streams() {
            let body = r#"{"jsonrpc":"2.0","id":1,"result":[]}"#;
            let bytes = frame(body);
            // a complete frame decodes to the exact body, consuming all of it.
            let (got, used) = parse_frame(&bytes).unwrap();
            assert_eq!(got, body);
            assert_eq!(used, bytes.len());
            // a partial buffer (one byte short) yields None until more arrives.
            assert!(parse_frame(&bytes[..bytes.len() - 1]).is_none());
            // two frames back to back: decode one, then the next from the remainder.
            let mut two = frame(body);
            two.extend_from_slice(&frame(body));
            let (_, n) = parse_frame(&two).unwrap();
            let (got2, _) = parse_frame(&two[n..]).unwrap();
            assert_eq!(got2, body);
        }

        #[test]
        fn references_request_and_response_roundtrip() {
            let req = references_request(7, "file:///x/a.rs", 40, 4);
            let v: Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "textDocument/references");
            assert_eq!(v["id"], 7);
            assert_eq!(v["params"]["position"]["line"], 40);
            // a result with two locations → 1-based (line+1) (uri, line) pairs.
            let resp = json!({
                "jsonrpc":"2.0","id":7,
                "result":[
                    {"uri":"file:///x/b.rs","range":{"start":{"line":9,"character":2}}},
                    {"uri":"file:///x/c.rs","range":{"start":{"line":0,"character":0}}}
                ]
            })
            .to_string();
            let refs = parse_references(&resp, 7).unwrap();
            assert_eq!(
                refs,
                vec![("file:///x/b.rs".into(), 10), ("file:///x/c.rs".into(), 1)]
            );
            // wrong id or an error response → None (caller degrades).
            assert!(parse_references(&resp, 99).is_none());
            let err =
                json!({"jsonrpc":"2.0","id":7,"error":{"code":-1,"message":"nope"}}).to_string();
            assert!(parse_references(&err, 7).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_provider_degrades_honestly() {
        let p = NullProvider;
        assert!(p.references("src/a.rs", "foo").is_none());
        assert_eq!(p.tier(), Tier::None);
        assert_eq!(Tier::Partial.label(), "lexical-approx");
        assert_eq!(Tier::Full.label(), "lsp");
    }
}
