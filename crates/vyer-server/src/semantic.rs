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

/// A real language-server-backed provider (Phase 2c): spawns an OPERATOR-configured
/// server, drives a `textDocument/references` exchange via [`lsp::drive_references`]
/// on a TIMEOUT-bounded worker thread (kill-on-timeout, so a hung/slow server can
/// never block a query — Rule §8), and maps the resulting `file://` URIs back to
/// repo-relative paths. The server command is operator-set (never request-supplied),
/// same trust model as `verify_cmd`/`code_run` (Rule §3). Any failure → `None`, so
/// the engine degrades to its lexical approximation.
///
/// Note (perf): this spawns per call, which is correct but slow for a real server's
/// cold init — the long-lived-server pool is the next optimization (Phase 2d). The
/// process I/O + timeout + degradation are CI-tested here; the happy path against a
/// real server (rust-analyzer) is validated locally (CI has no server binary).
pub struct LspProvider {
    pub argv: Vec<String>,
    pub root: std::path::PathBuf,
    pub lang_id: String,
    pub timeout: std::time::Duration,
}

impl LspProvider {
    /// Type-resolved references to the symbol at `(line, character)` (0-based) in the
    /// repo-relative `file_rel`. `None` on any failure → caller degrades.
    pub fn references_at(
        &self,
        file_rel: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<(String, u32)>> {
        use std::process::{Command, Stdio};
        let (prog, rest) = self.argv.split_first()?;
        let mut child = Command::new(prog)
            .args(rest)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?; // spawn failure (no binary) → degrade
        let mut stdin = child.stdin.take()?;
        let mut stdout = child.stdout.take()?;
        let root_disp = self.root.display().to_string();
        let root_uri = format!("file://{root_disp}");
        let file_uri = format!("file://{root_disp}/{file_rel}");
        let text = std::fs::read_to_string(self.root.join(file_rel)).unwrap_or_default();
        let lang = self.lang_id.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // Drive the (blocking) exchange on a worker so the MAIN thread can enforce a
        // hard timeout and kill a hung server — the query never blocks indefinitely.
        let handle = std::thread::spawn(move || {
            let r = lsp::drive_references(
                &mut stdout,
                &mut stdin,
                &root_uri,
                &file_uri,
                &lang,
                &text,
                line,
                character,
            );
            let _ = tx.send(r);
        });
        let result = rx.recv_timeout(self.timeout).ok().flatten();
        let _ = child.kill();
        let _ = handle.join();
        let prefix = format!("file://{root_disp}/");
        result.map(|refs| {
            refs.into_iter()
                .filter_map(|(uri, l)| uri.strip_prefix(&prefix).map(|rel| (rel.to_string(), l)))
                .collect()
        })
    }
}

/// LSP wire protocol (Phase 2a): the pure, fully-tested transport core a real
/// `LspProvider` (Phase 2b: spawn server + handshake) builds on. Kept here so the
/// hard correctness parts — Content-Length framing, JSON-RPC id correlation, the
/// `textDocument/references` request/response shapes — are unit-tested WITHOUT a
/// running language server (the happy path needs the server binary; this doesn't).
pub mod lsp {
    use serde_json::{json, Value};
    use std::io::{Read, Write};

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

    /// Read framed messages from `reader` until one is a JSON-RPC response whose
    /// `id` is `want_id`, returning its body. Interleaved notifications / other ids
    /// (log messages, progress, diagnostics a server emits during a request) are
    /// skipped — the part naive clients get wrong. `None` on EOF/garbage/cap. The
    /// `cap` bounds the buffer so a flood can't OOM us (Rule §8 robustness).
    fn read_until_id<R: Read>(
        reader: &mut R,
        buf: &mut Vec<u8>,
        want_id: i64,
        cap: usize,
    ) -> Option<String> {
        loop {
            while let Some((body, used)) = parse_frame(buf) {
                buf.drain(..used);
                if serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_i64))
                    == Some(want_id)
                {
                    return Some(body);
                }
                // else: a notification or a different id → skip, keep draining.
            }
            if buf.len() > cap {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match reader.read(&mut chunk) {
                Ok(0) => return None, // EOF without a matching response
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        }
    }

    /// Drive a full `textDocument/references` exchange over an already-connected
    /// server (`reader`/`writer` = the server's stdout/stdin): initialize →
    /// initialized → didOpen → references, skipping interleaved notifications, and
    /// return type-resolved `(uri, 1-based line)` pairs. Generic over the streams so
    /// it's unit-tested with in-memory mocks (no process, no hang); the process-spawn
    /// wrapper (Phase 2c) just hands it a child's piped stdio + a read timeout. Any
    /// protocol failure → `None`, so the engine degrades to its approximation (§8).
    #[allow(clippy::too_many_arguments)]
    pub fn drive_references<R: Read, W: Write>(
        reader: &mut R,
        writer: &mut W,
        root_uri: &str,
        file_uri: &str,
        lang_id: &str,
        text: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<(String, u32)>> {
        let init = json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"processId":null,"rootUri":root_uri,"capabilities":{}}
        })
        .to_string();
        writer.write_all(&frame(&init)).ok()?;
        let mut buf: Vec<u8> = Vec::new();
        read_until_id(reader, &mut buf, 1, 1 << 20)?; // await the initialize result
        for notif in [
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}).to_string(),
            json!({
                "jsonrpc":"2.0","method":"textDocument/didOpen",
                "params":{"textDocument":{"uri":file_uri,"languageId":lang_id,"version":1,"text":text}}
            })
            .to_string(),
        ] {
            writer.write_all(&frame(&notif)).ok()?;
        }
        writer
            .write_all(&frame(&references_request(2, file_uri, line, character)))
            .ok()?;
        writer.flush().ok()?;
        let body = read_until_id(reader, &mut buf, 2, 4 << 20)?;
        parse_references(&body, 2)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn drive_references_handshakes_skips_notifications_and_parses() {
            // a server that emits init result, an interleaved log notification (no id,
            // must be skipped), then the references result.
            let init_resp =
                json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}).to_string();
            let log = json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"indexing"}}).to_string();
            let refs_resp = json!({
                "jsonrpc":"2.0","id":2,
                "result":[{"uri":"file:///r/b.rs","range":{"start":{"line":4,"character":0}}}]
            })
            .to_string();
            let mut server_out = Vec::new();
            for m in [&init_resp, &log, &refs_resp] {
                server_out.extend_from_slice(&frame(m));
            }
            let mut reader = std::io::Cursor::new(server_out);
            let mut writer: Vec<u8> = Vec::new();
            let refs = drive_references(
                &mut reader,
                &mut writer,
                "file:///r",
                "file:///r/a.rs",
                "rust",
                "fn a(){}",
                0,
                3,
            )
            .unwrap();
            assert_eq!(refs, vec![("file:///r/b.rs".to_string(), 5)]); // 0-based → 1-based
                                                                       // the client sent the full handshake, in order.
            let sent = String::from_utf8(writer).unwrap();
            for needle in [
                "\"initialize\"",
                "\"initialized\"",
                "textDocument/didOpen",
                "textDocument/references",
            ] {
                assert!(sent.contains(needle), "missing {needle} in: {sent}");
            }
        }

        #[test]
        fn drive_references_degrades_on_eof() {
            // server closes after the init result, never answering references → None.
            let init_resp = json!({"jsonrpc":"2.0","id":1,"result":{}}).to_string();
            let mut reader = std::io::Cursor::new(frame(&init_resp));
            let mut writer: Vec<u8> = Vec::new();
            assert!(drive_references(
                &mut reader,
                &mut writer,
                "file:///r",
                "file:///r/a.rs",
                "rust",
                "",
                0,
                0
            )
            .is_none());
        }

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

    #[test]
    fn lsp_provider_degrades_when_server_is_missing() {
        // SCRY-150: a non-existent server binary → spawn fails → None (no panic, no
        // hang), so the engine falls back to its approximation.
        let p = LspProvider {
            argv: vec!["vyer_definitely_no_such_server_xyz".into()],
            root: std::env::temp_dir(),
            lang_id: "rust".into(),
            timeout: std::time::Duration::from_millis(500),
        };
        assert!(p.references_at("a.rs", 0, 0).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn lsp_provider_degrades_when_server_says_nothing() {
        // SCRY-150: a server that exits immediately (`true`) → EOF before any LSP
        // response → None, promptly, via the real child pipe (no hang).
        let p = LspProvider {
            argv: vec!["true".into()],
            root: std::env::temp_dir(),
            lang_id: "rust".into(),
            timeout: std::time::Duration::from_secs(2),
        };
        assert!(p.references_at("a.rs", 0, 0).is_none());
    }
}
