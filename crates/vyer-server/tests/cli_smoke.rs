//! CLI smoke test: runs the real `vyer` binary's `query` subcommand against a
//! hermetic temp fixture and asserts each detail view is well-formed. Locks the
//! CLI + flag-parsing surface (incl. the positional-query fix) into `cargo test`
//! so a regression fails the suite, not just `scripts/smoke.sh`.

use std::path::Path;
use std::process::Command;

fn vyer_query(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_vyer"))
        .arg("query")
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .expect("failed to run vyer binary");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

#[test]
fn cli_query_surface_smoke() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn validate_token(t: &str) -> bool {\n    !t.is_empty()\n}\n",
    )
    .unwrap();

    // search returns the envelope
    let s = vyer_query(dir.path(), &["validate_token", "--detail", "snippet"]);
    assert!(s.contains("code/result"), "search envelope: {s}");
    assert!(s.contains("validate_token"), "search hit: {s}");

    // count / tree views
    assert!(
        vyer_query(dir.path(), &["fn", "--detail", "count"]).contains("matches on"),
        "count view"
    );
    assert!(
        vyer_query(dir.path(), &["", "--detail", "tree"]).contains("files"),
        "tree view"
    );

    // read-by-path with a line range
    assert!(
        vyer_query(dir.path(), &["--path", "lib.rs", "--lines", "-2"]).contains("1: "),
        "read head view"
    );

    // detail=ast with NO positional query — the positional-fix regression guard.
    // Before the fix, the `--path` value was misread as the query.
    let ast = vyer_query(dir.path(), &["--path", "lib.rs", "--detail", "ast"]);
    assert!(
        ast.contains("(source_file"),
        "ast dump should run with no positional query: {ast}"
    );
    assert!(
        !ast.contains("missing search string"),
        "positional-query parsing regressed: {ast}"
    );
}

fn vyer_apply(root: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_vyer"))
        .arg("apply")
        .args(args)
        .arg("--root")
        .arg(root)
        .output()
        .expect("failed to run vyer apply");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

#[test]
fn cli_apply_rename_dry_run_and_undo_message() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn old_name() -> u8 {\n    1\n}\nfn caller() -> u8 {\n    old_name()\n}\n",
    )
    .unwrap();

    // rename dry-run (no --write) reports occurrences and writes nothing.
    let out = vyer_apply(
        dir.path(),
        &["--locator", "lib.rs#old_name", "--rename", "new_name"],
    );
    assert!(
        out.contains("rename") && out.contains("occurrence"),
        "rename report: {out}"
    );
    let on_disk = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
    assert!(
        on_disk.contains("old_name") && !on_disk.contains("new_name"),
        "dry-run must not write: {on_disk}"
    );

    // @delete is body-less over the CLI (SCRY-051) — a diff/dry-run, not the
    // "no new_body" error.
    let del = vyer_apply(dir.path(), &["--locator", "lib.rs#@delete:old_name"]);
    assert!(
        !del.contains("no new_body"),
        "@delete must not demand a body over the CLI: {del}"
    );
    assert!(
        del.contains("@@") || del.contains("dry run"),
        "@delete should produce a diff: {del}"
    );

    // --undo over the CLI explains it's a live-session (MCP) feature, not a crash.
    let undo = vyer_apply(dir.path(), &["--undo", "1"]);
    assert!(
        undo.contains("same running session") || undo.contains("MCP daemon"),
        "undo should explain the session limitation: {undo}"
    );
}
