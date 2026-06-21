//! A localhost-only, bearer-token-gated HTTP transport for the MCP JSON-RPC
//! dispatch. This is the optional network surface, and it is locked down by
//! construction (Rule §3 / §9):
//!   * binds **127.0.0.1 only** — any non-loopback bind is refused outright
//!     (the MCPJam CVSS-9.8 `0.0.0.0` lesson);
//!   * every request must carry `Authorization: Bearer <token>` — constant-time
//!     compared; a miss is `401` with no body;
//!   * it speaks the same [`crate::jsonrpc`] dispatch as everything else.
//!
//! Implemented over `std::net` (no axum/tokio needed here) so it stays small,
//! auditable, and synchronously testable: a test binds an ephemeral loopback
//! port, connects a `TcpStream`, and asserts both the 401 path and a real
//! `tools/call` round-trip.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::Value;

use crate::engine::Engine;
use crate::jsonrpc;

/// Bind a loopback listener. Refuses any non-loopback address. The caller then
/// drives it with [`serve`] (blocking) or accepts connections itself.
pub fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to bind a non-loopback address; HTTP is localhost-only",
        ));
    }
    TcpListener::bind(addr)
}

/// Serve forever, handling each connection in turn. `token` is the required
/// bearer credential. (Single-threaded accept loop: requests are short and the
/// engine is fast; a thread pool is a trivial later addition.)
pub fn serve(listener: TcpListener, engine: Arc<Engine>, token: String) -> std::io::Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                // One bad client must never take down the server.
                let _ = handle_connection(s, &engine, &token);
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Handle a single HTTP request/response. Public for integration tests.
pub fn handle_connection(
    mut stream: TcpStream,
    engine: &Arc<Engine>,
    token: &str,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // ---- request line ----
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let _path = parts.next().unwrap_or("");

    // ---- headers ----
    let mut content_length = 0usize;
    let mut authorized = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            } else if name == "authorization" {
                if let Some(presented) = value.strip_prefix("Bearer ") {
                    authorized = constant_time_eq(presented.as_bytes(), token.as_bytes());
                }
            }
        }
    }

    if method != "POST" {
        return write_response(&mut stream, 405, "Method Not Allowed", None);
    }
    if !authorized {
        // No body — don't leak whether the token was close.
        return write_response(&mut stream, 401, "Unauthorized", None);
    }

    // ---- body ----
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let response_body = match serde_json::from_slice::<Value>(&body) {
        Ok(msg) => jsonrpc::handle(engine, &msg)
            .map(|v| v.to_string())
            .unwrap_or_default(), // notification => empty 204-ish body
        Err(e) => serde_json::json!({
            "jsonrpc": "2.0", "id": Value::Null,
            "error": { "code": -32700, "message": format!("parse error: {e}") }
        })
        .to_string(),
    };

    write_response(&mut stream, 200, "OK", Some(&response_body))
}

fn write_response(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: Option<&str>,
) -> std::io::Result<()> {
    let body = body.unwrap_or("");
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Length-independent-ish constant-time comparison to avoid a timing oracle on
/// the bearer token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
