//! End-to-end proof over the *real* wire: spawn the compiled `vyer serve`
//! binary and speak the MCP stdio protocol (newline-delimited JSON-RPC) to it,
//! exactly as an agent host would. This validates the rmcp transport, the tool
//! router, param deserialization, and the engine — the whole stack, no mocks.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn fixture_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn validate_token(tok: &str) -> bool {\n    !tok.is_empty()\n}\n",
    )
    .unwrap();
    dir
}

/// Read newline-delimited lines off the child's stdout on a background thread so
/// the test can apply a timeout (never hang CI if the server misbehaves).
fn spawn_reader(stdout: ChildStdout) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn next_json(rx: &mpsc::Receiver<String>) -> serde_json::Value {
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for a server response");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return v;
        }
        // ignore any non-JSON noise (there shouldn't be any on stdout)
    }
}

fn send(stdin: &mut std::process::ChildStdin, msg: &serde_json::Value) {
    let line = serde_json::to_string(msg).unwrap();
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_stdio_server_handshake_and_tools_call() {
    let dir = fixture_root();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vyer"))
        .args(["serve", "--root", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vyer binary");

    let mut stdin = child.stdin.take().unwrap();
    let rx = spawn_reader(child.stdout.take().unwrap());
    let _guard = Kill(child); // ensure the server is killed even on panic

    // 1) initialize handshake
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},
                      "clientInfo":{"name":"e2e-test","version":"0"}}
        }),
    );
    let init = next_json(&rx);
    assert_eq!(init["id"], 1, "init response id");
    assert_eq!(
        init["result"]["serverInfo"]["name"], "vyer",
        "server name over the wire: {init}"
    );

    // 2) initialized notification (no response expected)
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );

    // 3) tools/list — the two-tool surface
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let list = next_json(&rx);
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"code".to_string()),
        "tools/list must expose `code`: {names:?}"
    );
    assert!(
        names.contains(&"code_apply".to_string()),
        "tools/list must expose `code_apply`"
    );
    // The code_apply input schema exposes the new `word` field (scoped local
    // rename, SCRY-046) so a reconnecting agent can actually use it over MCP.
    let apply_tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "code_apply")
        .expect("code_apply tool");
    let schema = apply_tool["inputSchema"].to_string();
    assert!(
        schema.contains("word"),
        "code_apply schema must expose the `word` field: {schema}"
    );

    // 4) tools/call code — a real search returns the UNTRUSTED envelope
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"code","arguments":{"queries":[{"q":"validate_token","detail":"snippet"}]}}
        }),
    );
    let call = next_json(&rx);
    assert_eq!(call["id"], 3);
    let text = call["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("source=UNTRUSTED"),
        "envelope over the wire: {text}"
    );
    assert!(text.contains("validate_token"), "search hit over the wire");

    // 4b) tools/call code detail=ast (SP-13) — the AST dump arrives over the wire
    // with node kinds AND field labels (so mode=ast queries are author-able).
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"code","arguments":{"queries":[{"path":"src/lib.rs","detail":"ast"}]}}
        }),
    );
    let ast = next_json(&rx);
    assert_eq!(ast["id"], 6);
    let ast_text = ast["result"]["content"][0]["text"]
        .as_str()
        .expect("ast text");
    assert!(
        ast_text.contains("function_item"),
        "detail=ast node kinds over the wire: {ast_text}"
    );
    assert!(
        ast_text.contains("name: (identifier)"),
        "detail=ast field labels over the wire: {ast_text}"
    );

    // 5) resources/list + resources/read over the wire (MCP Resources)
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}),
    );
    let rlist = next_json(&rx);
    let uris: Vec<String> = rlist["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(
        uris.contains(&"vyer://status".to_string())
            && uris.contains(&"vyer://playbook".to_string()),
        "resources/list over the wire (status + playbook): {uris:?}"
    );

    // The agent playbook is readable over the real rmcp transport (the default
    // reconnect path) — "documentation for the mcp", served through the mcp.
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":60,"method":"resources/read",
                            "params":{"uri":"vyer://playbook"}}),
    );
    let pb = next_json(&rx);
    let pb_text = pb["result"]["contents"][0]["text"]
        .as_str()
        .expect("playbook text");
    assert!(
        pb_text.contains("agent playbook") && pb_text.contains("intent"),
        "playbook resource over the wire should serve the guide"
    );

    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":5,"method":"resources/read",
                            "params":{"uri":"vyer://status"}}),
    );
    let rread = next_json(&rx);
    let status_text = rread["result"]["contents"][0]["text"]
        .as_str()
        .expect("status text");
    assert!(
        status_text.contains("vyer/status"),
        "status resource over the wire: {status_text}"
    );

    // 6) RED-TEAM: code_apply over the wire is REFUSED without --allow-writes
    // (this server was started read-only) — the Rule §9 gate, enforced over the
    // real protocol, and the file on disk stays untouched.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"code_apply","arguments":{"edits":[{
                "locator":"src/lib.rs#validate_token",
                "new_body":"pub fn validate_token() -> bool { true }"
            }]}}
        }),
    );
    let denied = next_json(&rx);
    assert_eq!(denied["id"], 7);
    let denied_text = denied["result"]["content"][0]["text"]
        .as_str()
        .expect("apply-denied text");
    assert!(
        denied_text.contains("writes are disabled") || denied_text.contains("--allow-writes"),
        "code_apply must be refused without --allow-writes: {denied_text}"
    );
    let on_disk = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        on_disk.contains("!tok.is_empty()") && !on_disk.contains("-> bool { true }"),
        "file must be unchanged after a denied apply: {on_disk}"
    );
}

#[test]
fn real_stdio_apply_and_diff_over_the_wire() {
    // The WRITE path over the actual MCP protocol (not just in-process): a gated
    // code_apply lands on disk, and detail=diff reports it — over the wire.
    let dir = fixture_root();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vyer"))
        .args([
            "serve",
            "--root",
            dir.path().to_str().unwrap(),
            "--allow-writes",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn vyer binary");
    let mut stdin = child.stdin.take().unwrap();
    let rx = spawn_reader(child.stdout.take().unwrap());
    let _guard = Kill(child);

    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},
                      "clientInfo":{"name":"e2e-write","version":"0"}}
        }),
    );
    let _ = next_json(&rx);
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );

    // code_apply over the wire — the gated, sandboxed write path.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"code_apply","arguments":{"edits":[{
                "locator":"src/lib.rs#validate_token",
                "new_body":"pub fn validate_token(tok: &str) -> bool {\n    tok.len() > 3\n}"
            }]}}
        }),
    );
    let applied = next_json(&rx);
    assert_eq!(applied["id"], 2);
    let apply_text = applied["result"]["content"][0]["text"]
        .as_str()
        .expect("apply text");
    assert!(
        apply_text.contains("@@") || apply_text.contains("written"),
        "apply diff/confirmation over the wire: {apply_text}"
    );

    // The write really hit disk.
    let on_disk = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        on_disk.contains("tok.len() > 3"),
        "write not on disk: {on_disk}"
    );

    // detail=diff over the wire reports the session edit.
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"code","arguments":{"queries":[{"detail":"diff"}]}}
        }),
    );
    let diff = next_json(&rx);
    assert_eq!(diff["id"], 3);
    let diff_text = diff["result"]["content"][0]["text"]
        .as_str()
        .expect("diff text");
    assert!(
        diff_text.contains("src/lib.rs"),
        "session diff names the edited file: {diff_text}"
    );
    assert!(
        diff_text.contains("tok.len() > 3"),
        "session diff shows the new body: {diff_text}"
    );

    // RED-TEAM (Rule §9): even WITH --allow-writes, the sandbox refuses a path
    // escape, `mcp.json`, and `.git/hooks` — over the real protocol — and creates
    // no file. (`validate_write` returns DenyReason → "write denied".)
    for (i, bad) in ["../evil.rs#x", "mcp.json#x", ".git/hooks/pre-commit#x"]
        .iter()
        .enumerate()
    {
        send(
            &mut stdin,
            &serde_json::json!({
                "jsonrpc":"2.0","id": 10 + i as i64,"method":"tools/call",
                "params":{"name":"code_apply","arguments":{"edits":[{
                    "locator": bad, "new_body": "x\n"
                }]}}
            }),
        );
        let resp = next_json(&rx);
        let t = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("denied text");
        assert!(
            t.contains("write denied"),
            "forbidden target `{bad}` must be refused over the wire: {t}"
        );
    }
    assert!(
        !dir.path().join("mcp.json").exists(),
        "mcp.json must not be created"
    );
    assert!(
        !dir.path().parent().unwrap().join("evil.rs").exists(),
        "path-escape file must not be created"
    );
}
