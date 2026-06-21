//! Integration + red-team tests for the Vyer MCP server.
//!
//! These exercise the whole engine over a real on-disk fixture repo (no mocks)
//! and assert the non-negotiable invariants from CLAUDE.md §1:
//!   * well-formed, UNTRUSTED-marked, edge-ordered output envelope;
//!   * read-after-write freshness (staleness = 0) through the apply path;
//!   * the apply path is deterministic and rejects parse-breaking edits;
//!   * security: writes are gated, and the sandbox refuses escapes / mcp.json /
//!     .git/hooks — there is no command-execution surface at all;
//!   * the shared MCP JSON-RPC dispatch and the localhost+token HTTP transport
//!     behave (auth required, loopback-only, real tools/call round-trip).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use vyer_server::engine::{ApplyRequest, CodeRequest, Edit, Engine, EngineConfig, Query};
use vyer_server::{http, jsonrpc};

// ---- fixture ----------------------------------------------------------------

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    std::fs::write(
        root.join("src/auth/token.rs"),
        "pub fn validate_token(tok: &str) -> bool {\n    !tok.is_empty()\n}\n\npub fn refresh(tok: &str) -> bool {\n    true\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/auth/login.rs"),
        "pub fn login(user: &str) -> bool {\n    user.len() > 0\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "# fixture\nvalidate_token lives in auth.\n",
    )
    .unwrap();
    (dir, root)
}

fn engine(root: &Path, allow_writes: bool) -> Engine {
    let mut cfg = EngineConfig::new(root.to_path_buf());
    cfg.allow_writes = allow_writes;
    Engine::new(cfg).unwrap()
}

fn one(q: &str, mode: &str, detail: &str) -> CodeRequest {
    CodeRequest {
        queries: vec![Query {
            q: q.into(),
            path: None,
            mode: mode.into(),
            detail: detail.into(),
            path_scope: vec![],
            lang: None,
            lines: None,
            all_of: vec![],
            any_of: vec![],
            none_of: vec![],
            k: 8,
        }],
        budget_tokens: 8000,
        exclude_seen: false,
    }
}

fn read(path: &str, detail: &str) -> CodeRequest {
    CodeRequest {
        queries: vec![Query {
            q: String::new(),
            path: Some(path.into()),
            mode: "auto".into(),
            detail: detail.into(),
            path_scope: vec![],
            lang: None,
            lines: None,
            all_of: vec![],
            any_of: vec![],
            none_of: vec![],
            k: 8,
        }],
        budget_tokens: 8000,
        exclude_seen: false,
    }
}

// ---- read-by-path (SCRY-003) ------------------------------------------------

#[test]
fn read_by_path_returns_whole_file_without_a_query() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    // Suffix resolution: `token.rs` → `src/auth/token.rs`.
    let out = eng.code(&read("token.rs", "full"));
    assert!(out.contains("source=UNTRUSTED"), "envelope: {out}");
    // The full file is returned, line-numbered — both functions present.
    assert!(out.contains("validate_token"), "missing body: {out}");
    assert!(out.contains("refresh"), "whole file expected: {out}");
    assert!(
        out.contains("1: pub fn validate_token"),
        "line numbers: {out}"
    );
}

#[test]
fn read_by_path_outline_lists_symbols_and_locate_summarizes() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let outline = eng.code(&read("src/auth/token.rs", "outline"));
    assert!(outline.contains("validate_token") && outline.contains("refresh"));
    let locate = eng.code(&read("src/auth/token.rs", "locate"));
    assert!(
        locate.contains("lines") && locate.contains("symbols"),
        "{locate}"
    );
}

#[test]
fn read_by_path_unknown_file_is_reported() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let out = eng.code(&read("does_not_exist.rs", "full"));
    assert!(
        out.contains("not indexed"),
        "expected a not-indexed note: {out}"
    );
}

// ---- authoring: anchored / insert / create (SCRY-004/013/014/002) -----------

fn one_edit(
    locator: &str,
    anchor: Option<&str>,
    replace: Option<&str>,
    body: Option<&str>,
) -> ApplyRequest {
    ApplyRequest {
        edits: vec![Edit {
            locator: locator.into(),
            anchor: anchor.map(Into::into),
            replace: replace.map(Into::into),
            new_body: body.map(Into::into),
            lazy_edit: None,
            rename: None,
            move_to: None,
            word: false,
            path_scope: vec![],
        }],
        dry_run: false,
        undo: None,
    }
}

#[test]
fn apply_anchored_edit_replaces_unique_text() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/auth/login.rs#login",
        Some("user.len() > 0"),
        Some("!user.is_empty()"),
        None,
    ));
    assert!(out.is_ok(), "{out:?}");
    let read = eng.code(&read("src/auth/login.rs", "full"));
    assert!(
        read.contains("!user.is_empty()"),
        "anchored edit not applied: {read}"
    );
}

#[test]
fn apply_anchored_file_scope_edits_module_level() {
    // SCRY-002: a bare-path locator scopes the anchor to the whole file, so a
    // top-level line (here, prepending a `use`) becomes editable.
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/auth/token.rs",
        Some("pub fn validate_token"),
        Some("use std::fmt;\n\npub fn validate_token"),
        None,
    ));
    assert!(out.is_ok(), "{out:?}");
    let read = eng.code(&read("src/auth/token.rs", "full"));
    assert!(
        read.contains("use std::fmt;"),
        "module-level edit not applied: {read}"
    );
}

#[test]
fn apply_insert_new_symbol_is_searchable() {
    // SCRY-013: insert a brand-new function after an existing one.
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/auth/login.rs#@after:login",
        None,
        None,
        Some("pub fn logout() -> bool {\n    true\n}"),
    ));
    assert!(out.is_ok(), "{out:?}");
    let q = eng.code(&one("logout", "structural", "locate"));
    assert!(
        q.contains("#logout@"),
        "inserted symbol must be searchable: {q}"
    );
}

#[test]
fn apply_create_new_file_then_search_it() {
    // SCRY-014: author a whole new file via `PATH#@new`.
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/util.rs#@new",
        None,
        None,
        Some("pub fn helper() -> u8 {\n    7\n}\n"),
    ));
    assert!(out.is_ok(), "{out:?}");
    assert!(
        root.join("src/util.rs").exists(),
        "file not created on disk"
    );
    let q = eng.code(&one("helper", "structural", "locate"));
    assert!(
        q.contains("#helper@"),
        "new file's symbol must be searchable: {q}"
    );
}

#[test]
fn apply_create_refuses_existing_file() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/auth/login.rs#@new",
        None,
        None,
        Some("pub fn x() {}\n"),
    ));
    assert!(
        out.is_err(),
        "creating over an existing file must be refused"
    );
}

// ---- delete / class-qualified / diagnostics / outline (SCRY-026/005/006/017) -

#[test]
fn apply_delete_symbol_removes_it() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit(
        "src/auth/token.rs#@delete:refresh",
        None,
        None,
        None,
    ));
    assert!(out.is_ok(), "{out:?}");
    let read = eng.code(&read("src/auth/token.rs", "full"));
    assert!(
        !read.contains("fn refresh"),
        "deleted symbol still present: {read}"
    );
    assert!(
        read.contains("validate_token"),
        "sibling symbol must survive"
    );
}

#[test]
fn delete_on_disk_file_absent_from_index() {
    // SCRY-115: a file present on disk but missing from the warm index (it
    // predated indexing, was gitignored, or the watcher missed it) must still be
    // deletable — otherwise code_apply fails all-or-nothing and the agent falls
    // back to native tools. Regression for the "@delete: not indexed" report.
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    // Created on disk AFTER indexing -> present on disk, absent from the warm core.
    std::fs::write(root.join("src/orphan.rs"), "fn orphan() {}\n").unwrap();
    let out = eng.code_apply(&one_edit("src/orphan.rs#@delete", None, None, None));
    assert!(
        out.is_ok(),
        "delete of on-disk-but-unindexed file should succeed: {out:?}"
    );
    assert!(
        !root.join("src/orphan.rs").exists(),
        "file should be gone from disk"
    );
}

#[test]
fn edit_loads_on_disk_file_absent_from_index() {
    // SCRY-115: the same gap for an EDIT (not just delete) — an anchor edit on a
    // file present on disk but missing from the index pulls it in on demand
    // instead of failing "file not indexed".
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    std::fs::write(
        root.join("src/orphan.rs"),
        "fn orphan() { let a = 1; }\n",
    )
    .unwrap();
    let out = eng.code_apply(&one_edit(
        "src/orphan.rs",
        Some("let a = 1;"),
        Some("let a = 2;"),
        None,
    ));
    assert!(
        out.is_ok(),
        "edit of on-disk-but-unindexed file should succeed: {out:?}"
    );
    let read = eng.code(&read("src/orphan.rs", "full"));
    assert!(read.contains("let a = 2;"), "edit not applied: {read}");
}

#[test]
fn apply_delete_file_removes_it() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&one_edit("src/auth/login.rs#@delete", None, None, None));
    assert!(out.is_ok(), "{out:?}");
    assert!(
        !root.join("src/auth/login.rs").exists(),
        "file not deleted on disk"
    );
    // And it's gone from the index too (no stale results).
    let q = eng.code(&one("login", "structural", "locate"));
    assert!(
        !q.contains("login.rs#login@"),
        "deleted symbol still indexed: {q}"
    );
}

#[test]
fn apply_miss_lists_candidate_symbols() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&one_edit("src/auth/token.rs#nope", None, None, Some("x")))
        .unwrap_err();
    assert!(
        err.contains("symbols in"),
        "miss should list candidates: {err}"
    );
    assert!(
        err.contains("validate_token"),
        "candidate names expected: {err}"
    );
}

fn class_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("a.py"),
        "class A:\n    def m(self):\n        return 1\n\nclass B:\n    def m(self):\n        return 2\n",
    )
    .unwrap();
    (dir, root)
}

#[test]
fn class_qualified_locator_disambiguates_methods() {
    let (_d, root) = class_fixture();
    let eng = engine(&root, true);
    // Bare `m` is ambiguous (A.m and B.m); `B.m` resolves precisely.
    let out = eng.code_apply(&one_edit(
        "a.py#B.m",
        None,
        None,
        Some("    def m(self):\n        return 99"),
    ));
    assert!(out.is_ok(), "{out:?}");
    let read = eng.code(&read("a.py", "full"));
    assert!(read.contains("return 99"), "B.m not edited: {read}");
    assert!(read.contains("return 1"), "A.m must be untouched: {read}");
}

#[test]
fn outline_of_class_lists_members() {
    let (_d, root) = class_fixture();
    let eng = engine(&root, false);
    let out = eng.code(&one("A", "structural", "outline"));
    assert!(out.contains("class A"), "{out}");
    assert!(
        out.contains("def m"),
        "class outline must list members: {out}"
    );
}

#[test]
fn semantic_mode_finds_symbol_from_a_conceptual_query() {
    // SP-7: subword TF-IDF retrieval — a natural-language query (not the exact
    // identifier) should still surface `validate_token`.
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let out = eng.code(&one(
        "check whether the auth token is valid",
        "semantic",
        "locate",
    ));
    assert!(
        out.contains("validate_token"),
        "semantic retrieval failed: {out}"
    );
}

// ---- SUPERPOWER: repo-wide rename (SCRY-027) --------------------------------

fn rename_req(locator: &str, to: &str) -> ApplyRequest {
    ApplyRequest {
        edits: vec![Edit {
            locator: locator.into(),
            rename: Some(to.into()),
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    }
}

#[test]
fn rename_updates_definition_and_cross_file_references() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    // `validate_token` is defined in token.rs and referenced in README.md.
    let out = eng
        .code_apply(&rename_req(
            "src/auth/token.rs#validate_token",
            "verify_token",
        ))
        .unwrap();
    assert!(
        out.contains("rename `validate_token` → `verify_token`"),
        "{out}"
    );
    let tok = eng.code(&read("src/auth/token.rs", "full"));
    assert!(
        tok.contains("verify_token") && !tok.contains("validate_token"),
        "def: {tok}"
    );
    let readme = eng.code(&read("README.md", "full"));
    assert!(
        readme.contains("verify_token"),
        "cross-file reference not renamed: {readme}"
    );
}

#[test]
fn rename_to_keyword_is_rejected() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&rename_req("src/auth/token.rs#validate_token", "fn"));
    assert!(
        out.is_err(),
        "renaming to a reserved keyword must be rejected: {out:?}"
    );
    assert!(eng
        .code(&read("src/auth/token.rs", "full"))
        .contains("validate_token"));
}

#[test]
fn rename_unknown_symbol_errors_and_changes_nothing() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let out = eng.code_apply(&rename_req("src/auth/token.rs#nope_qqq", "whatever"));
    assert!(
        out.is_err(),
        "renaming a missing symbol must error: {out:?}"
    );
    let tok = eng.code(&read("src/auth/token.rs", "full"));
    assert!(tok.contains("validate_token"), "no file may change: {tok}");
}

// ---- SP-3: impact / blast-radius -------------------------------------------

#[test]
fn impact_shows_transitive_referrers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("a.py"),
        "def base():\n    return 1\n\ndef mid():\n    return base()\n\ndef top():\n    return mid()\n",
    )
    .unwrap();
    let eng = engine(&root, false);
    let out = eng.code(&one("base", "auto", "impact"));
    assert!(out.contains("impact of `base`"), "{out}");
    assert!(out.contains("#mid"), "direct caller `mid` missing: {out}");
    assert!(
        out.contains("#top"),
        "transitive caller `top` missing: {out}"
    );
}

// ---- SP-4: bulk structural search-replace ----------------------------------

#[test]
fn bulk_replace_across_glob_is_atomic_and_validated() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![Edit {
            locator: "src/**".into(),
            anchor: Some("-> bool".into()),
            replace: Some("-> bool /*checked*/".into()),
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    };
    let out = eng.code_apply(&req).unwrap();
    assert!(out.contains("bulk replace"), "{out}");
    assert!(eng
        .code(&read("src/auth/login.rs", "full"))
        .contains("/*checked*/"));
    assert!(eng
        .code(&read("src/auth/token.rs", "full"))
        .contains("/*checked*/"));
}

// ---- SP-5: move symbol across files ----------------------------------------

#[test]
fn move_symbol_to_another_file() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![Edit {
            locator: "src/auth/token.rs#refresh".into(),
            move_to: Some("src/auth/moved.rs".into()),
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    };
    let out = eng.code_apply(&req).unwrap();
    assert!(out.contains("move `refresh`"), "{out}");
    assert!(
        !eng.code(&read("src/auth/token.rs", "full"))
            .contains("fn refresh"),
        "still in source"
    );
    let q = eng.code(&one("refresh", "structural", "locate"));
    assert!(
        q.contains("moved.rs#refresh@"),
        "moved symbol not searchable in dest: {q}"
    );
}

// ---- SP-6: undo -------------------------------------------------------------

fn undo_req(n: usize) -> ApplyRequest {
    ApplyRequest {
        edits: vec![],
        dry_run: false,
        undo: Some(n),
    }
}

#[test]
fn undo_reverts_the_last_apply() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit(
        "src/auth/login.rs#login",
        None,
        None,
        Some("pub fn login(u: &str) -> bool {\n    !u.is_empty()\n}"),
    ))
    .unwrap();
    assert!(eng
        .code(&read("src/auth/login.rs", "full"))
        .contains("is_empty"));
    let out = eng.code_apply(&undo_req(1)).unwrap();
    assert!(out.contains("reverted"), "{out}");
    let read_back = eng.code(&read("src/auth/login.rs", "full"));
    assert!(
        read_back.contains("user.len() > 0") && !read_back.contains("is_empty"),
        "{read_back}"
    );
    let on_disk = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    assert!(
        on_disk.contains("user.len() > 0"),
        "undo not flushed to disk: {on_disk}"
    );
}

#[test]
fn undo_recreates_a_deleted_file() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit("src/auth/login.rs#@delete", None, None, None))
        .unwrap();
    assert!(!root.join("src/auth/login.rs").exists());
    eng.code_apply(&undo_req(1)).unwrap();
    assert!(
        root.join("src/auth/login.rs").exists(),
        "undo must recreate the deleted file"
    );
}

#[test]
fn undo_with_empty_history_errors() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    assert!(eng.code_apply(&undo_req(1)).is_err());
}

// ---- SP-9: one-call context pack -------------------------------------------

#[test]
fn context_pack_assembles_def_callees_callers_and_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("auth.py"),
        "def validate_token(tok):\n    return check_len(tok)\n\ndef check_len(t):\n    return len(t) > 8\n",
    )
    .unwrap();
    std::fs::write(
        root.join("api.py"),
        "from auth import validate_token\n\ndef handle_login(tok):\n    return validate_token(tok)\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests.py"),
        "from auth import validate_token\n\ndef test_token():\n    assert validate_token('xxxxxxxxx')\n",
    )
    .unwrap();
    let eng = engine(&root, false);
    let out = eng.code(&one("validate_token", "auto", "context"));
    assert!(
        out.contains("def validate_token"),
        "definition missing: {out}"
    );
    assert!(out.contains("check_len"), "callee missing: {out}");
    assert!(out.contains("handle_login"), "caller missing: {out}");
    assert!(
        out.contains("test_token") && out.contains("(test)"),
        "test detection missing: {out}"
    );
}

// ---- SP-7: AST-pattern structural search -----------------------------------

#[test]
fn ast_query_finds_structural_matches_and_reports_bad_queries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("a.py"),
        "def f(x):\n    return g(x)\n\nclass W:\n    def m(self):\n        return 1\n",
    )
    .unwrap();
    let eng = engine(&root, false);
    // structural: find class definitions by AST node, not text
    let out = eng.code(&one(
        "(class_definition name: (identifier) @c)",
        "ast",
        "locate",
    ));
    assert!(out.contains("class W"), "AST class query failed: {out}");
    // an invalid query is reported (not a silent empty result — fixes SCRY-016)
    let bad = eng.code(&one("(nonsense_node", "ast", "locate"));
    assert!(
        bad.contains("AST query"),
        "invalid query should be reported: {bad}"
    );
}

// ---- SP-2: atomic multi-edit transactions ----------------------------------

#[test]
fn multi_edit_batch_is_atomic_on_failure() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![
            // edit 1: valid — would rewrite login.rs
            Edit {
                locator: "src/auth/login.rs#login".into(),
                new_body: Some(
                    "pub fn login(user: &str) -> bool {\n    !user.is_empty()\n}".into(),
                ),
                ..Default::default()
            },
            // edit 2: invalid symbol — the whole batch must abort
            Edit {
                locator: "src/auth/token.rs#does_not_exist".into(),
                new_body: Some("pub fn x() {}".into()),
                ..Default::default()
            },
        ],
        dry_run: false,
        undo: None,
    };
    assert!(
        eng.code_apply(&req).is_err(),
        "batch with a bad edit must fail"
    );
    // Edit 1 must be rolled back — warm core AND disk show the original.
    let read = eng.code(&read("src/auth/login.rs", "full"));
    assert!(
        read.contains("user.len() > 0"),
        "edit 1 not rolled back in core: {read}"
    );
    assert!(
        !read.contains("!user.is_empty()"),
        "edit 1 leaked despite abort: {read}"
    );
    let on_disk = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    assert!(
        on_disk.contains("user.len() > 0"),
        "edit 1 leaked to disk: {on_disk}"
    );
}

#[test]
fn multi_edit_batch_commits_all_on_success() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![
            Edit {
                locator: "src/auth/login.rs#login".into(),
                new_body: Some("pub fn login(u: &str) -> bool {\n    !u.is_empty()\n}".into()),
                ..Default::default()
            },
            Edit {
                locator: "src/auth/token.rs#refresh".into(),
                new_body: Some("pub fn refresh(tok: &str) -> bool {\n    false\n}".into()),
                ..Default::default()
            },
        ],
        dry_run: false,
        undo: None,
    };
    assert!(eng.code_apply(&req).is_ok());
    assert!(eng
        .code(&read("src/auth/login.rs", "full"))
        .contains("is_empty"));
    assert!(eng
        .code(&read("src/auth/token.rs", "full"))
        .contains("false"));
    // Both committed to disk.
    assert!(std::fs::read_to_string(root.join("src/auth/token.rs"))
        .unwrap()
        .contains("false"));
}

#[test]
fn multi_edit_batch_rolls_back_into_op() {
    // SP-2 atomicity must hold for the new ops too: a valid `@into` paired with a
    // failing edit leaves ZERO changes (disk + warm core).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/cfg.rs"),
        "pub struct Cfg {\n    pub a: u8,\n}\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![
            // valid @into — would add a field inside Cfg
            Edit {
                locator: "src/cfg.rs#@into:Cfg".into(),
                new_body: Some("    pub b: u16,".into()),
                ..Default::default()
            },
            // invalid symbol — the whole batch must abort
            Edit {
                locator: "src/cfg.rs#nonexistent".into(),
                new_body: Some("x".into()),
                ..Default::default()
            },
        ],
        dry_run: false,
        undo: None,
    };
    assert!(
        eng.code_apply(&req).is_err(),
        "batch with a bad edit must fail"
    );
    let on_disk = std::fs::read_to_string(root.join("src/cfg.rs")).unwrap();
    assert!(
        !on_disk.contains("pub b: u16,"),
        "@into leaked to disk despite batch abort: {on_disk}"
    );
    let core = eng.code(&read("src/cfg.rs", "full"));
    assert!(
        !core.contains("pub b: u16,"),
        "@into leaked in warm core despite abort: {core}"
    );
}

#[test]
fn scoped_word_rename_stays_in_one_symbol() {
    // SCRY-046: `count` is a local in BOTH `a` and `b`; a `word` rename scoped to
    // `a` must rename only a's occurrences — the safe local rename repo-wide
    // `rename` can't do.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/m.rs"),
        "fn a() -> u8 {\n    let count = 1;\n    count + count\n}\nfn b() -> u8 {\n    let count = 2;\n    count\n}\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    let report = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/m.rs#a".into(),
                anchor: Some("count".into()),
                replace: Some("total".into()),
                word: true,
                path_scope: vec![],
                ..Default::default()
            }],
            dry_run: false,
            undo: None,
        })
        .expect("scoped word rename should succeed");
    assert!(
        report.contains("@@") || report.contains("written"),
        "diff/confirm: {report}"
    );
    let on_disk = std::fs::read_to_string(root.join("src/m.rs")).unwrap();
    // a's occurrences renamed…
    assert!(
        on_disk.contains("let total = 1;") && on_disk.contains("total + total"),
        "a's locals not renamed: {on_disk}"
    );
    // …b's same-named local untouched (the safety guarantee)
    assert!(
        on_disk.contains("let count = 2;"),
        "b's local leaked into the rename: {on_disk}"
    );
}

#[test]
fn scoped_word_rename_works_in_python() {
    // SCRY-046 is language-agnostic (whole-word replace + per-language parse gate
    // + tree-sitter symbol spans). Verify the indentation-delimited Python case.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("m.py"),
        "def f():\n    tmp = 1\n    return tmp + tmp\ndef g():\n    tmp = 9\n    return tmp\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    eng.code_apply(&ApplyRequest {
        edits: vec![Edit {
            locator: "m.py#f".into(),
            anchor: Some("tmp".into()),
            replace: Some("acc".into()),
            word: true,
            path_scope: vec![],
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    })
    .expect("python scoped rename should succeed");
    let d = std::fs::read_to_string(root.join("m.py")).unwrap();
    assert!(
        d.contains("acc = 1") && d.contains("acc + acc"),
        "f's local not renamed: {d}"
    );
    assert!(
        d.contains("tmp = 9"),
        "g's local leaked into the rename: {d}"
    );
}

#[test]
fn read_by_path_refuses_outside_root() {
    // §9 (no reads outside declared project roots): read-by-path must never serve
    // a file outside the root — neither an absolute path nor `../` traversal — and
    // must leak no content (only indexed, in-root files are served).
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP-SECRET-DO-NOT-LEAK").unwrap();
    let abs = outside.path().join("secret.txt");

    let out = eng.code(&read(abs.to_str().unwrap(), "full"));
    assert!(
        !out.contains("TOP-SECRET"),
        "absolute outside-root path leaked content: {out}"
    );
    assert!(
        out.contains("not indexed") || out.contains("inside the repo root"),
        "outside-root absolute read should be refused: {out}"
    );

    let trav = eng.code(&read("../../../../../../etc/hosts", "full"));
    assert!(
        trav.contains("not indexed") || trav.contains("inside the repo root"),
        "`../` traversal read should be refused: {trav}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_to_outside_root_is_not_followed() {
    // §9: a symlink INSIDE the repo pointing OUTSIDE the root must not leak the
    // target's content — the `ignore` walker doesn't follow symlinks by default,
    // so the target is never indexed nor served by read-by-path.
    let (_d, root) = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "SECRET_OUTSIDE_ROOT").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt")).unwrap();

    // index AFTER the symlink exists (Engine::new walks the root).
    let eng = engine(&root, false);

    let searched = eng.code(&one("SECRET_OUTSIDE_ROOT", "lexical", "snippet"));
    assert!(
        !searched.contains("SECRET_OUTSIDE_ROOT"),
        "symlink target leaked via search: {searched}"
    );
    let read_out = eng.code(&read("link.txt", "full"));
    assert!(
        !read_out.contains("SECRET_OUTSIDE_ROOT"),
        "symlink target leaked via read-by-path: {read_out}"
    );
}

#[cfg(unix)]
#[test]
fn write_through_symlinked_dir_is_refused() {
    // SCRY-067 (§9): a symlinked DIRECTORY component can redirect a write OUTSIDE
    // the root even though the path string looks contained — the lexical sandbox
    // (pure vyer-core) can't see it. The apply path resolves symlinks and refuses
    // the escape, while legitimate writes still work (no over-blocking).
    let (_d, root) = fixture();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("outdir")).unwrap();
    let eng = engine(&root, true);

    let err = eng
        .code_apply(&one_edit(
            "outdir/evil.rs#@new",
            None,
            None,
            Some("pub fn evil() {}\n"),
        ))
        .unwrap_err();
    assert!(
        err.contains("outside the project root"),
        "a symlinked-dir write must be refused: {err}"
    );
    assert!(
        !outside.path().join("evil.rs").exists(),
        "escape file was created OUTSIDE the root"
    );

    // the gate must not over-block a legitimate in-root @new.
    let ok = eng.code_apply(&one_edit(
        "src/new.rs#@new",
        None,
        None,
        Some("pub fn ok() {}\n"),
    ));
    assert!(ok.is_ok(), "legitimate @new should still work: {ok:?}");
    assert!(root.join("src/new.rs").exists(), "legit file not created");
}

#[test]
fn stale_locator_blake3_is_rejected() {
    // SCRY-056: a locator's `:: blake3=HEX` guards against editing from a stale
    // read. A wrong hash (symbol changed since) is refused and writes nothing; a
    // hashless locator is never staleness-checked and applies normally.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("m.rs"), "fn target() {\n    1\n}\n").unwrap();
    let eng = engine(&root, true);

    let stale = "m.rs#target@L1-3 :: blake3=deadbeefdeadbeef";
    let err = eng
        .code_apply(&one_edit(stale, Some("1"), Some("2"), None))
        .unwrap_err();
    assert!(
        err.contains("stale locator"),
        "a wrong blake3 should be refused: {err}"
    );
    // nothing was written.
    let on_disk = std::fs::read_to_string(root.join("m.rs")).unwrap();
    assert!(
        on_disk.contains("    1\n"),
        "stale edit must not write: {on_disk}"
    );

    // a hashless locator is not staleness-checked → applies.
    let ok = eng.code_apply(&one_edit("m.rs#target", Some("1"), Some("2"), None));
    assert!(ok.is_ok(), "hashless locator should apply: {ok:?}");
}

#[test]
fn move_to_same_file_is_refused() {
    // SCRY-050: moving a symbol onto its own file would duplicate it (the dest
    // append reads the pre-cut text). Must be refused, not silently corrupt.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("m.rs"),
        "fn a() {\n    1\n}\nfn b() {\n    2\n}\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "m.rs#a".into(),
                move_to: Some("m.rs".into()),
                ..Default::default()
            }],
            dry_run: false,
            undo: None,
        })
        .unwrap_err();
    assert!(
        err.contains("same file") || err.contains("onto itself"),
        "same-file move should be refused: {err}"
    );
    // the file is untouched — the symbol is NOT duplicated.
    let on_disk = std::fs::read_to_string(root.join("m.rs")).unwrap();
    assert_eq!(
        on_disk.matches("fn a").count(),
        1,
        "symbol was duplicated by a same-file move: {on_disk}"
    );
}

#[test]
fn batch_create_then_into_same_file() {
    // Real workflow + intra-batch freshness: edit 1 creates a file, edit 2 adds a
    // member INTO the struct edit 1 just created — all atomic, in one batch.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let eng = engine(&root, true);
    eng.code_apply(&ApplyRequest {
        edits: vec![
            Edit {
                locator: "src/cfg.rs#@new".into(),
                new_body: Some("pub struct Cfg {\n    pub a: u8,\n}\n".into()),
                ..Default::default()
            },
            Edit {
                locator: "src/cfg.rs#@into:Cfg".into(),
                new_body: Some("    pub b: u16,".into()),
                ..Default::default()
            },
        ],
        dry_run: false,
        undo: None,
    })
    .expect("create-then-into batch should succeed (intra-batch freshness)");
    let on_disk = std::fs::read_to_string(root.join("src/cfg.rs")).unwrap();
    assert!(
        on_disk.contains("pub a: u8,") && on_disk.contains("pub b: u16,"),
        "both the created field and the @into field should be present: {on_disk}"
    );
    // b is inside the struct (before the closing brace).
    assert!(
        on_disk.find("pub b").unwrap() < on_disk.rfind('}').unwrap(),
        "@into field landed outside the struct: {on_disk}"
    );
}

#[test]
fn large_batch_applies_atomically_and_fresh() {
    // Write-path scale: many edits in ONE code_apply must all land atomically,
    // each resolving against the prior edits' warm-core state (intra-batch
    // freshness), and every result be queryable immediately after.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("m.rs"), "pub fn anchor_fn() {}\n").unwrap();
    let eng = engine(&root, true);

    // 20 new functions, each inserted after the previous one (chained @after).
    let edits: Vec<Edit> = (0..20)
        .map(|i| {
            let prev = if i == 0 {
                "anchor_fn".to_string()
            } else {
                format!("gen_{}", i - 1)
            };
            Edit {
                locator: format!("m.rs#@after:{prev}"),
                new_body: Some(format!("pub fn gen_{i}() -> u8 {{\n    {i}\n}}")),
                ..Default::default()
            }
        })
        .collect();
    eng.code_apply(&ApplyRequest {
        edits,
        dry_run: false,
        undo: None,
    })
    .expect("large chained batch should apply");

    // all 20 generated symbols are present, valid, and immediately searchable.
    let on_disk = std::fs::read_to_string(root.join("m.rs")).unwrap();
    for i in 0..20 {
        assert!(
            on_disk.contains(&format!("fn gen_{i}(")),
            "edit {i} missing from disk"
        );
    }
    let q = eng.code(&one("gen_19", "structural", "locate"));
    assert!(
        q.contains("#gen_19@"),
        "last batched symbol not query-fresh: {q}"
    );
}

#[test]
fn conflicting_same_symbol_edits_in_batch_roll_back() {
    // Two edits to the SAME symbol in one batch: edit 1 replaces its body, edit 2
    // anchors on the OLD content (now gone, via intra-batch freshness) → edit 2
    // fails → the WHOLE batch rolls back atomically (no partial/corrupt state).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("m.rs"), "fn target() {\n    let old = 1;\n}\n").unwrap();
    let eng = engine(&root, true);
    let res = eng.code_apply(&ApplyRequest {
        edits: vec![
            Edit {
                locator: "m.rs#target".into(),
                new_body: Some("fn target() {\n    let renamed = 2;\n}".into()),
                ..Default::default()
            },
            Edit {
                locator: "m.rs#target".into(),
                anchor: Some("let old = 1;".into()),
                replace: Some("let x = 3;".into()),
                ..Default::default()
            },
        ],
        dry_run: false,
        undo: None,
    });
    assert!(
        res.is_err(),
        "edit 2 anchors on content edit 1 removed — should fail"
    );
    // atomic: the file is UNCHANGED (edit 1 rolled back too, disk untouched).
    let on_disk = std::fs::read_to_string(root.join("m.rs")).unwrap();
    assert!(
        on_disk.contains("let old = 1;") && !on_disk.contains("renamed"),
        "a conflicting batch must roll back fully: {on_disk}"
    );
}

#[cfg(unix)]
#[test]
fn flush_failure_does_not_leave_warm_core_ahead_of_disk() {
    // Rule §2 consistency: if the DISK write fails (here: a read-only file), the
    // warm core must not be left AHEAD of disk — a query after the failed apply
    // must see the ORIGINAL on-disk content, never the un-persisted edit.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let f = root.join("ro.rs");
    std::fs::write(&f, "fn target() {\n    1\n}\n").unwrap();
    let mut perms = std::fs::metadata(&f).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&f, perms).unwrap();

    let eng = engine(&root, true);
    let res = eng.code_apply(&one_edit("ro.rs#target", Some("1"), Some("99"), None));
    assert!(
        res.is_err(),
        "writing a read-only file should fail: {res:?}"
    );
    // CRITICAL: the warm core must reflect DISK (no `99`), not the failed edit.
    let q = eng.code(&one("target", "structural", "snippet"));
    assert!(
        !q.contains("99"),
        "warm core left AHEAD of disk after flush failure (Rule §2): {q}"
    );
}

#[cfg(unix)]
#[test]
fn move_into_readonly_dest_does_not_lose_the_symbol() {
    // Atomic apply (CLAUDE.md §1.2 / §11): a `move_to` flushes the source CUT and
    // the dest WRITE; if the dest write fails AFTER the source cut, the symbol
    // would be LOST (cut but not pasted). A failed move must change ZERO files.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("src.rs"),
        "fn mover() {\n    1\n}\nfn keep() {}\n",
    )
    .unwrap();
    let dest = root.join("dest.rs");
    std::fs::write(&dest, "fn existing() {}\n").unwrap();
    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&dest, perms).unwrap();

    let eng = engine(&root, true);
    let res = eng.code_apply(&ApplyRequest {
        edits: vec![Edit {
            locator: "src.rs#mover".into(),
            move_to: Some("dest.rs".into()),
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    });
    assert!(res.is_err(), "move into a read-only dest should fail");
    // CRITICAL: `mover` must NOT be lost — still in src.rs (the move aborted).
    let src = std::fs::read_to_string(root.join("src.rs")).unwrap();
    assert!(
        src.contains("fn mover"),
        "symbol LOST: cut from src but dest write failed — not atomic: {src}"
    );
}

#[cfg(unix)]
#[test]
fn undo_into_readonly_file_is_refused_and_preserves_history() {
    // An undo whose restore target is read-only must be REFUSED cleanly with the
    // history INTACT — not pop the batch, fail the write, and lose the undo history
    // (which would leave the agent unable to recover).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let f = root.join("m.rs");
    std::fs::write(&f, "fn target() {\n    1\n}\n").unwrap();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit("m.rs#target", Some("1"), Some("2"), None))
        .unwrap();

    // make the file read-only so the undo restore would fail.
    let mut perms = std::fs::metadata(&f).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&f, perms).unwrap();
    let res = eng.code_apply(&ApplyRequest {
        edits: vec![],
        dry_run: false,
        undo: Some(1),
    });
    assert!(res.is_err(), "undo into a read-only file should be refused");

    // restore writability — the history must have been PRESERVED, so undo now works.
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&f).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&f, perms).unwrap();
    let res2 = eng.code_apply(&ApplyRequest {
        edits: vec![],
        dry_run: false,
        undo: Some(1),
    });
    assert!(
        res2.is_ok(),
        "undo history should have been preserved: {res2:?}"
    );
    assert!(
        std::fs::read_to_string(&f).unwrap().contains("    1\n"),
        "undo should restore the original body"
    );
}

#[test]
fn delete_and_move_carry_attributes_and_docs() {
    // SCRY-075: deleting or moving a symbol must take its OWN attributes/doc
    // comments — orphaning a `#[inline]` re-decorates the next item (compile
    // error); a stranded `/// doc` is wrong too.
    let (_d, root) = fixture();
    std::fs::write(
        root.join("m.rs"),
        "//! module doc\n/// Doc for foo.\n#[inline]\nfn foo() -> i32 { 1 }\n\nfn keep() {}\n",
    )
    .unwrap();
    // write the move fixtures BEFORE the engine indexes the root.
    std::fs::write(
        root.join("a.rs"),
        "/// Doc.\n#[inline]\nfn mover() -> i32 { 7 }\nfn stay() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("b.rs"), "fn existing() {}\n").unwrap();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit("m.rs#@delete:foo", None, None, None))
        .unwrap();
    let after = std::fs::read_to_string(root.join("m.rs")).unwrap();
    assert!(!after.contains("Doc for foo"), "doc orphaned: {after}");
    assert!(!after.contains("inline"), "attribute orphaned: {after}");
    assert!(after.contains("fn keep"), "keep must remain");
    // the INNER module doc `//!` is NOT the symbol's — it must survive.
    assert!(
        after.contains("//! module doc"),
        "module doc wrongly consumed"
    );

    // move carries the trivia to the destination (not orphaned, not lost).
    eng.code_apply(&ApplyRequest {
        edits: vec![Edit {
            locator: "a.rs#mover".into(),
            move_to: Some("b.rs".into()),
            ..Default::default()
        }],
        dry_run: false,
        undo: None,
    })
    .unwrap();
    let a = std::fs::read_to_string(root.join("a.rs")).unwrap();
    let b = std::fs::read_to_string(root.join("b.rs")).unwrap();
    assert!(
        !a.contains("inline") && !a.contains("Doc"),
        "trivia orphaned in source: {a}"
    );
    assert!(
        b.contains("inline") && b.contains("Doc") && b.contains("fn mover"),
        "trivia not carried to dest: {b}"
    );
}

#[test]
fn insert_before_keeps_symbol_attached_to_its_docs() {
    // SCRY-076: `@before` must insert ABOVE the symbol's own attributes/docs, not
    // between them and the symbol (which would split a `#[inline]`/`/// doc` off
    // onto the inserted item).
    let (_d, root) = fixture();
    std::fs::write(
        root.join("m.rs"),
        "/// Doc.\n#[inline]\nfn foo() -> i32 { 1 }\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit(
        "m.rs#@before:foo",
        None,
        None,
        Some("fn inserted() {}"),
    ))
    .unwrap();
    let after = std::fs::read_to_string(root.join("m.rs")).unwrap();
    let ins = after.find("fn inserted").unwrap();
    let doc = after.find("/// Doc.").unwrap();
    let attr = after.find("#[inline]").unwrap();
    let foo = after.find("fn foo").unwrap();
    assert!(ins < doc, "inserted must be ABOVE the doc: {after}");
    assert!(
        doc < attr && attr < foo,
        "doc + attribute must stay attached to foo: {after}"
    );
}

#[test]
fn concurrent_write_and_read_never_observes_half_applied() {
    // Rule §2 + §9: while one thread applies edits, queries from another thread
    // must always see a CONSISTENT committed body — never a half-applied or garbage
    // state. The warm-core Mutex serializes each write with concurrent reads.
    use std::sync::Arc;
    use std::thread;
    let (_d, root) = fixture();
    std::fs::write(root.join("m.rs"), "fn target() { let v = 0; }\n").unwrap();
    let eng = Arc::new(engine(&root, true));

    let writer = {
        let eng = Arc::clone(&eng);
        thread::spawn(move || {
            for i in 0..200 {
                let v = i % 2;
                let _ = eng.code_apply(&one_edit(
                    "m.rs#target",
                    None,
                    None,
                    Some(&format!("fn target() {{ let v = {v}; }}")),
                ));
            }
        })
    };

    // every read must see a complete, committed body (v=0 or v=1), never a mix.
    for _ in 0..600 {
        let out = eng.code(&one("target", "structural", "snippet"));
        assert!(
            out.contains("let v = 0") || out.contains("let v = 1"),
            "a concurrent read observed an inconsistent body: {out}"
        );
    }
    writer.join().unwrap();
}

#[test]
fn apply_preserves_crlf_line_endings() {
    // SCRY-102: editing a CRLF (Windows) file must NOT introduce mixed LF/CRLF — the
    // spliced new body's `\n` is normalized to the file's `\r\n` convention so the
    // diff stays a one-line change, not a whole-file EOL churn.
    let (_d, root) = fixture();
    std::fs::write(
        root.join("w.rs"),
        "fn alpha() {\r\n    let x = 1;\r\n}\r\nfn beta() {}\r\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    eng.code_apply(&one_edit(
        "w.rs#alpha",
        None,
        None,
        Some("fn alpha() {\n    let y = 2;\n}"),
    ))
    .unwrap();
    let out = std::fs::read_to_string(root.join("w.rs")).unwrap();
    // every `\n` is part of a `\r\n` — no lone LF leaked from the spliced body.
    assert_eq!(
        out.matches('\n').count(),
        out.matches("\r\n").count(),
        "mixed EOLs after editing a CRLF file: {out:?}"
    );
    assert!(out.contains("let y = 2"), "edit not applied: {out:?}");
}

#[test]
fn rename_can_be_path_scoped_for_monorepos() {
    // SCRY-105: a repo-wide rename can be confined to a package via `path_scope`, so a
    // monorepo's same-named-but-DISTINCT symbol in another package is left alone
    // (still symbol-aware, unlike a text bulk-replace).
    let (_d, root) = fixture();
    std::fs::create_dir_all(root.join("packages/auth/src")).unwrap();
    std::fs::create_dir_all(root.join("packages/billing/src")).unwrap();
    std::fs::write(root.join("packages/auth/src/lib.rs"), "fn handler() {}\n").unwrap();
    std::fs::write(
        root.join("packages/billing/src/lib.rs"),
        "fn handler() {}\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![Edit {
            locator: "packages/auth/src/lib.rs#handler".into(),
            anchor: None,
            replace: None,
            new_body: None,
            lazy_edit: None,
            rename: Some("handle".into()),
            move_to: None,
            word: false,
            path_scope: vec!["packages/auth/**".into()],
        }],
        dry_run: false,
        undo: None,
    };
    eng.code_apply(&req).unwrap();
    let auth = std::fs::read_to_string(root.join("packages/auth/src/lib.rs")).unwrap();
    let billing = std::fs::read_to_string(root.join("packages/billing/src/lib.rs")).unwrap();
    assert!(
        auth.contains("fn handle("),
        "auth package not renamed: {auth:?}"
    );
    assert!(
        billing.contains("fn handler("),
        "billing wrongly renamed — path_scope ignored: {billing:?}"
    );
}

#[test]
fn misplaced_at_directive_gives_a_helpful_hint() {
    // SCRY-081: a misplaced @-directive (`foo#@delete` instead of `@delete:foo`)
    // parses as a symbol named `foo#@delete` — guide to the correct syntax rather
    // than fall through to a confusing "no new_body" error.
    let (_d, root) = fixture();
    std::fs::write(root.join("m.rs"), "fn foo() {}\nfn keep() {}\n").unwrap();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&one_edit("m.rs#foo#@delete", None, None, None))
        .unwrap_err();
    assert!(
        err.contains("RIGHT AFTER") && err.contains("@delete:foo"),
        "unhelpful error for a misplaced @-directive: {err}"
    );
    // the malformed op changed nothing.
    assert!(
        std::fs::read_to_string(root.join("m.rs"))
            .unwrap()
            .contains("fn foo"),
        "foo should be intact"
    );
}

#[test]
fn ambiguous_symbol_error_lists_candidate_ranges() {
    // SCRY-082: when a symbol name is ambiguous (a Rust `struct` + its `impl`),
    // the error LISTS the candidate `@L` ranges so the agent disambiguates
    // directly, instead of re-querying for the line numbers (saves a round-trip).
    let (_d, root) = fixture();
    std::fs::write(
        root.join("m.rs"),
        "struct Cfg {\n    timeout: u64,\n}\nimpl Cfg {\n    fn new() {}\n}\n",
    )
    .unwrap();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&one_edit(
            "m.rs#@into:Cfg",
            None,
            None,
            Some("    retries: u32,"),
        ))
        .unwrap_err();
    assert!(
        err.contains("ambiguous") && err.contains("@L1-3") && err.contains("@L4-6"),
        "the ambiguity error must list the candidate @L ranges: {err}"
    );
    // the agent can retry directly with a range from the hint.
    let ok = eng.code_apply(&one_edit(
        "m.rs#@into:Cfg@L1-3",
        None,
        None,
        Some("    retries: u32,"),
    ));
    assert!(ok.is_ok(), "a disambiguated retry should work: {ok:?}");
}

#[test]
fn rename_dry_run_reports_but_does_not_write() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let req = ApplyRequest {
        edits: vec![Edit {
            locator: "src/auth/token.rs#validate_token".into(),
            rename: Some("verify_token".into()),
            ..Default::default()
        }],
        dry_run: true,
        undo: None,
    };
    let out = eng.code_apply(&req).unwrap();
    assert!(
        out.contains("dry_run") && out.contains("occurrence"),
        "{out}"
    );
    // Nothing written: the original name is still on disk.
    let tok = eng.code(&read("src/auth/token.rs", "full"));
    assert!(
        tok.contains("validate_token") && !tok.contains("verify_token"),
        "{tok}"
    );
}

// ---- search -----------------------------------------------------------------

#[test]
fn code_search_returns_well_formed_untrusted_envelope() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let out = eng.code(&one("validate_token", "auto", "snippet"));
    assert!(
        out.starts_with("\u{27E6}code/result v1\u{27E7}"),
        "envelope header missing: {out}"
    );
    assert!(
        out.contains("source=UNTRUSTED"),
        "provenance marker missing"
    );
    assert!(out.contains("validate_token"), "expected hit body");
    // locator is symbol-anchored and carries a content hash for staleness checks.
    assert!(
        out.contains("#validate_token@L1-3"),
        "symbol-anchored locator missing: {out}"
    );
    assert!(out.contains("blake3="), "content hash missing from locator");
}

#[test]
fn no_match_returns_actionable_error_envelope() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let out = eng.code(&one("zzz_no_such_symbol_anywhere_qqq", "auto", "snippet"));
    assert!(
        out.starts_with("\u{27E6}code/error v1\u{27E7}"),
        "expected error envelope: {out}"
    );
    assert!(out.contains("PATTERN_NO_MATCH"));
    assert!(out.contains("hint:"), "error must carry an actionable hint");
}

#[test]
fn structural_mode_finds_symbol_by_name() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let out = eng.code(&one("login", "structural", "outline"));
    assert!(
        out.contains("login"),
        "structural search should find the login symbol: {out}"
    );
}

#[test]
fn tree_sitter_parsing_is_active_in_server() {
    // A method nested inside an `impl` block, plus a closing brace hidden in a
    // string literal — both cases the heuristic scanner gets wrong. If the
    // engine resolves `compute` to its true node span, tree-sitter is live.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("m.rs"),
        "pub struct Calc;\nimpl Calc {\n    pub fn compute(&self) -> i32 {\n        let s = \"}\";\n        let _ = s;\n        42\n    }\n}\n",
    )
    .unwrap();
    let eng = engine(&root, false);
    let out = eng.code(&one("compute", "structural", "snippet"));
    // node span is lines 3..=7 (the heuristic would mis-end at the string's `}`).
    assert!(
        out.contains("#compute@L3-7"),
        "tree-sitter node span expected: {out}"
    );
    assert!(
        out.contains("42"),
        "full method body should be in the snippet: {out}"
    );
}

#[test]
fn path_scope_and_lang_filters_apply() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let mut req = one("validate_token", "lexical", "locate");
    req.queries[0].path_scope = vec!["src/auth/**".into()];
    req.queries[0].lang = Some("rust".into());
    let out = eng.code(&req);
    assert!(out.contains("src/auth/token.rs"));
    // README (markdown) is excluded by the rust lang filter even though it
    // mentions validate_token.
    assert!(
        !out.contains("README.md"),
        "lang filter should exclude markdown: {out}"
    );
}

#[test]
fn exclude_seen_suppresses_repeats_across_calls() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    let mut req = one("validate_token", "lexical", "snippet");
    req.exclude_seen = true;
    let first = eng.code(&req);
    assert!(first.contains("validate_token"));
    let second = eng.code(&req);
    // already-seen span is dropped on the second identical call.
    assert!(
        !second.contains("#validate_token@L1-3"),
        "second call should exclude seen span: {second}"
    );
}

// ---- apply path + freshness -------------------------------------------------

#[test]
fn deterministic_apply_writes_and_is_immediately_fresh() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let report = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/auth/login.rs#login".into(),
                anchor: None,
                replace: None,
                new_body: Some(
                    "pub fn login(user: &str) -> bool {\n    !user.is_empty()\n}".into(),
                ),
                lazy_edit: None,
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: false,
            undo: None,
        })
        .expect("apply should succeed with writes enabled");
    assert!(report.contains("@@"), "unified diff expected");
    assert!(report.contains("written"), "should confirm write");

    // disk really changed
    let on_disk = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    assert!(
        on_disk.contains("!user.is_empty()"),
        "file on disk not updated"
    );

    // FRESHNESS: a query issued right after the write sees the new body — no
    // stale cache can serve the pre-edit code (CLAUDE.md Rule §2).
    let out = eng.code(&one("is_empty", "lexical", "snippet"));
    assert!(
        out.contains("!user.is_empty()"),
        "read-after-write staleness detected: {out}"
    );
}

#[test]
fn apply_into_container_writes_and_is_fresh() {
    // SP-12 e2e: `@into:` adds a field inside a struct, the write lands before
    // the closing brace, and a query right after sees it (read-after-write).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/cfg.rs"),
        "pub struct Cfg {\n    pub a: u8,\n}\n",
    )
    .unwrap();
    let eng = engine(&root, true);

    let report = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/cfg.rs#@into:Cfg".into(),
                anchor: None,
                replace: None,
                new_body: Some("    pub b: u16,".into()),
                lazy_edit: None,
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: false,
            undo: None,
        })
        .expect("@into apply should succeed with writes enabled");
    assert!(report.contains("written"), "should confirm write: {report}");

    // Disk: the new field is INSIDE the struct (before its closing brace).
    let on_disk = std::fs::read_to_string(root.join("src/cfg.rs")).unwrap();
    assert!(
        on_disk.contains("pub b: u16,"),
        "field not written: {on_disk}"
    );
    let bi = on_disk.find("pub b").unwrap();
    let last_brace = on_disk.rfind('}').unwrap();
    assert!(
        bi < last_brace,
        "field landed outside the struct: {on_disk}"
    );
    assert!(
        on_disk.find("pub a").unwrap() < bi,
        "field order wrong: {on_disk}"
    );

    // FRESHNESS: a query right after the write sees the new field (Rule §2).
    let out = eng.code(&one("pub b", "lexical", "snippet"));
    assert!(
        out.contains("pub b: u16,"),
        "read-after-write staleness after @into: {out}"
    );
}

#[test]
fn dry_run_does_not_write() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let before = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    let report = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/auth/login.rs#login".into(),
                anchor: None,
                replace: None,
                new_body: Some("pub fn login(user: &str) -> bool {\n    false\n}".into()),
                lazy_edit: None,
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: true,
            undo: None,
        })
        .unwrap();
    assert!(report.contains("dry_run"));
    let after = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    assert_eq!(before, after, "dry_run must not write");
}

#[test]
fn apply_rejects_parse_breaking_edit() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/auth/login.rs#login".into(),
                anchor: None,
                replace: None,
                new_body: Some("pub fn login(user: &str) -> bool {\n    true\n".into()), // missing }
                lazy_edit: None,
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: false,
            undo: None,
        })
        .unwrap_err();
    assert!(
        err.contains("does not parse"),
        "expected re-parse rejection, got: {err}"
    );
    // and the file must be untouched
    let on_disk = std::fs::read_to_string(root.join("src/auth/login.rs")).unwrap();
    assert!(
        on_disk.contains("user.len() > 0"),
        "rejected edit must not have written"
    );
}

// ---- security / red team ----------------------------------------------------

#[test]
fn writes_are_gated_when_not_allowed() {
    let (_d, root) = fixture();
    let eng = engine(&root, false); // allow_writes = false
    let err = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/auth/login.rs#login".into(),
                anchor: None,
                replace: None,
                new_body: Some("pub fn login() -> bool { true }".into()),
                lazy_edit: None,
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: false,
            undo: None,
        })
        .unwrap_err();
    assert!(
        err.contains("writes are disabled"),
        "gating message expected: {err}"
    );
}

#[test]
fn sandbox_refuses_escape_and_sensitive_targets() {
    let (_d, root) = fixture();
    let eng = engine(&root, true); // even WITH writes enabled, the sandbox holds
    for bad in [
        "../evil.rs#x",
        "src/../../etc/passwd#x",
        "mcp.json#x",
        ".git/hooks/pre-commit#x",
    ] {
        let err = eng
            .code_apply(&ApplyRequest {
                edits: vec![Edit {
                    locator: bad.into(),
                    anchor: None,
                    replace: None,
                    new_body: Some("x".into()),
                    lazy_edit: None,
                    rename: None,
                    move_to: None,
                    word: false,
                    path_scope: vec![],
                }],
                dry_run: false,
                undo: None,
            })
            .unwrap_err();
        assert!(
            err.contains("write denied"),
            "sandbox must refuse `{bad}`, got: {err}"
        );
    }
    // None of these may have created a file anywhere.
    assert!(!root.join("mcp.json").exists());
    assert!(!root.join("evil.rs").exists());
}

#[test]
fn lazy_edit_is_not_silently_misapplied() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let err = eng
        .code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/auth/login.rs#login".into(),
                anchor: None,
                replace: None,
                new_body: None,
                lazy_edit: Some("// ... existing code ...\nreturn true;".into()),
                rename: None,
                move_to: None,
                word: false,
                path_scope: vec![],
            }],
            dry_run: false,
            undo: None,
        })
        .unwrap_err();
    assert!(
        err.contains("Phase-6"),
        "lazy_edit should be refused with a hint, got: {err}"
    );
}

#[test]
fn audit_log_records_every_call() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let _ = eng.code(&one("validate_token", "auto", "locate"));
    let _ = eng.code_apply(&ApplyRequest {
        edits: vec![Edit {
            locator: "src/auth/login.rs#login".into(),
            anchor: None,
            replace: None,
            new_body: Some("pub fn login(user: &str) -> bool {\n    true\n}".into()),
            lazy_edit: None,
            rename: None,
            move_to: None,
            word: false,
            path_scope: vec![],
        }],
        dry_run: true,
        undo: None,
    });
    let log = eng.audit_log();
    assert_eq!(log.len(), 2, "every code/code_apply call must be audited");
    assert_eq!(log[0].tool, "code");
    assert_eq!(log[1].tool, "code_apply");
}

#[test]
fn audit_log_persists_to_file() {
    // §9: the audit trail must persist to the operator's --audit FILE (not just in
    // memory) so calls survive the process and are externally reviewable.
    let (_d, root) = fixture();
    let audit_dir = tempfile::tempdir().unwrap();
    let audit = audit_dir.path().join("audit.log");
    let mut cfg = EngineConfig::new(root.clone());
    cfg.allow_writes = true;
    cfg.audit_path = Some(audit.clone());
    let eng = Engine::new(cfg).unwrap();

    let _ = eng.code(&one("validate_token", "auto", "locate"));
    let _ = eng.code_apply(&one_edit(
        "src/auth/login.rs#login",
        Some("user.len() > 0"),
        Some("!user.is_empty()"),
        None,
    ));
    let logged = std::fs::read_to_string(&audit).expect("audit file should be written");
    assert!(
        logged.contains("code\t"),
        "code call not persisted to file: {logged}"
    );
    assert!(
        logged.contains("code_apply\t"),
        "apply not persisted to file: {logged}"
    );
}

#[test]
fn jsonrpc_handles_malformed_input_gracefully() {
    // Daemon robustness: malformed JSON-RPC (unknown method, wrong-typed args,
    // unknown tool) must yield a clean error response, never a panic; a
    // notification (no id) gets no response.
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));

    let unknown_method = jsonrpc::handle(&eng, &json!({"id": 1, "method": "bogus"})).unwrap();
    assert_eq!(
        unknown_method["error"]["code"].as_i64(),
        Some(-32601),
        "unknown method: {unknown_method}"
    );

    let bad_args = jsonrpc::handle(
        &eng,
        &json!({"id": 2, "method": "tools/call",
                "params": {"name": "code", "arguments": {"queries": 42}}}),
    )
    .unwrap();
    assert_eq!(
        bad_args["error"]["code"].as_i64(),
        Some(-32602),
        "wrong-typed args: {bad_args}"
    );

    let unknown_tool = jsonrpc::handle(
        &eng,
        &json!({"id": 3, "method": "tools/call", "params": {"name": "evil"}}),
    )
    .unwrap();
    assert_eq!(
        unknown_tool["error"]["code"].as_i64(),
        Some(-32602),
        "unknown tool: {unknown_tool}"
    );

    // a notification (no `id`) gets no response — and no panic.
    assert!(jsonrpc::handle(&eng, &json!({"method": "ping"})).is_none());
}

#[test]
fn jsonrpc_serves_the_playbook_resource() {
    // "Documentation for the mcp": the agent usage playbook is a read-only MCP
    // resource (vyer://playbook) — listed and readable over the wire.
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));

    let list = jsonrpc::handle(&eng, &json!({"id": 1, "method": "resources/list"})).unwrap();
    assert!(
        list.to_string().contains("vyer://playbook"),
        "playbook not listed: {list}"
    );

    let read = jsonrpc::handle(
        &eng,
        &json!({"id": 2, "method": "resources/read",
                "params": {"uri": "vyer://playbook"}}),
    )
    .unwrap();
    let rs = read.to_string();
    assert!(
        rs.contains("agent playbook") && rs.contains("intent"),
        "playbook content not served over the resource"
    );

    // Security: resources/read is a FIXED allow-list (no dynamic path), so a
    // malicious URI can't traverse to an arbitrary file — it's refused.
    for evil in [
        "file:///etc/passwd",
        "vyer://../../etc/passwd",
        "vyer://playbook/../../../etc/passwd",
    ] {
        let r = jsonrpc::handle(
            &eng,
            &json!({"id": 9, "method": "resources/read", "params": {"uri": evil}}),
        )
        .unwrap();
        assert_eq!(
            r["error"]["code"].as_i64(),
            Some(-32602),
            "malicious resource uri {evil} must be refused: {r}"
        );
        assert!(
            !r.to_string().contains("root:"),
            "resource read leaked file content for {evil}"
        );
    }
}

// ---- MCP JSON-RPC dispatch (the shared wire contract) ----------------------

#[test]
fn jsonrpc_initialize_and_tools_list() {
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));

    let init =
        jsonrpc::handle(&eng, &json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "vyer");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    let list =
        jsonrpc::handle(&eng, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["code", "code_apply"],
        "exactly the two-tool surface"
    );
    // each tool advertises a JSON-Schema object for its params.
    assert!(tools[0]["inputSchema"].is_object());
}

#[test]
fn jsonrpc_tools_call_code_returns_envelope() {
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));
    let resp = jsonrpc::handle(
        &eng,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"code","arguments":{"queries":[{"q":"validate_token"}]}}}),
    )
    .unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("source=UNTRUSTED"));
    assert_eq!(resp["result"]["isError"], json!(false));
}

#[test]
fn jsonrpc_notification_gets_no_reply() {
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));
    let r = jsonrpc::handle(
        &eng,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    assert!(r.is_none(), "notifications must not get a response");
}

// ---- external-edit re-index -------------------------------------------------

#[test]
fn reindex_picks_up_an_external_edit() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);

    // Before: searching for a not-yet-present symbol finds nothing.
    let before = eng.code(&one("externally_added", "lexical", "snippet"));
    assert!(
        !before.contains("externally_added"),
        "symbol should not exist yet"
    );

    // Simulate an edit made OUTSIDE code_apply (editor / git checkout).
    std::fs::write(
        root.join("src/auth/login.rs"),
        "pub fn login(user: &str) -> bool {\n    externally_added(user)\n}\n",
    )
    .unwrap();
    assert!(
        eng.reindex_path("src/auth/login.rs"),
        "reindex should succeed"
    );

    // After re-index: the new content is query-ready.
    let after = eng.code(&one("externally_added", "lexical", "snippet"));
    assert!(
        after.contains("externally_added"),
        "re-index must make the edit visible: {after}"
    );
}

#[test]
fn reindex_purges_externally_removed_content() {
    // §2 (freshness=0), the DELETION direction: when an external edit REMOVES code
    // (e.g. a git checkout), a query after reindex must not still return the stale
    // content. Complements `reindex_picks_up_an_external_edit` (the add direction).
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    assert!(
        eng.code(&one("refresh", "lexical", "snippet"))
            .contains("pub fn refresh"),
        "refresh should exist initially"
    );

    // External edit drops `refresh`, leaving only validate_token.
    std::fs::write(
        root.join("src/auth/token.rs"),
        "pub fn validate_token(tok: &str) -> bool {\n    !tok.is_empty()\n}\n",
    )
    .unwrap();
    assert!(
        eng.reindex_path("src/auth/token.rs"),
        "reindex should succeed"
    );

    let after = eng.code(&one("refresh", "lexical", "snippet"));
    assert!(
        !after.contains("pub fn refresh"),
        "stale removed content returned after reindex: {after}"
    );
}

#[test]
fn reindex_all_purges_externally_deleted_files() {
    // §6 watcher: a file DELETED out-of-band (shell `rm`, `git checkout`) must
    // leave the index after a full rescan, not linger until restart. The walk
    // only ADDS/UPDATES present files; `index_repo` reconciles deletions by
    // dropping indexed paths that no longer exist (SCRY-114). Complements
    // `reindex_purges_externally_removed_content` (an emptied-but-present file).
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    assert!(
        eng.code(&one("login", "lexical", "snippet"))
            .contains("pub fn login"),
        "login should exist initially"
    );

    // Delete the whole file, outside vyer.
    std::fs::remove_file(root.join("src/auth/login.rs")).unwrap();
    eng.reindex_all().unwrap();

    let after = eng.code(&one("login", "lexical", "snippet"));
    assert!(
        !after.contains("pub fn login"),
        "deleted file's content still indexed after rescan: {after}"
    );
}

#[test]
fn build_and_vendor_dirs_are_not_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("target/debug/junk.rs"), "fn junk() {}\n").unwrap();
    std::fs::write(
        root.join("node_modules/pkg/index.js"),
        "function dep() {}\n",
    )
    .unwrap();
    let eng = engine(&root, false);
    let files = eng.indexed_files();
    assert!(
        files.contains(&"src/main.rs".to_string()),
        "source must be indexed: {files:?}"
    );
    assert!(
        !files
            .iter()
            .any(|f| f.starts_with("target/") || f.starts_with("node_modules/")),
        "build/vendor dirs must be pruned: {files:?}"
    );
}

#[test]
fn reindex_path_is_safe_for_missing_files() {
    let (_d, root) = fixture();
    let eng = engine(&root, false);
    assert!(
        !eng.reindex_path("does/not/exist.rs"),
        "missing file must not panic or index"
    );
}

// ---- graph refs (approximate) ----------------------------------------------

#[test]
fn refs_resolves_definition_and_cross_file_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/util.rs"),
        "pub fn helper() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn a() -> i32 {\n    helper() + helper()\n}\n",
    )
    .unwrap();
    let eng = engine(&root, false);

    let out = eng.code(&one("helper", "auto", "refs"));
    // definition span present, tagged with the honest tier.
    assert!(out.contains("def [fn]"), "definition span expected: {out}");
    assert!(
        out.contains("graph=partial(approx)"),
        "tier must be reported: {out}"
    );
    // references in the OTHER file are listed (and the def header line is not).
    assert!(
        out.contains("src/a.rs:2:"),
        "cross-file reference expected: {out}"
    );
    assert!(
        out.contains("references to `helper`"),
        "refs summary expected: {out}"
    );
}

// ---- repo map + resources ---------------------------------------------------

#[test]
fn repo_map_ranks_the_depended_upon_file_highest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // util.rs defines `shared_helper`; the other two files call it -> util is the hub.
    std::fs::write(
        root.join("src/util.rs"),
        "pub fn shared_helper() -> i32 {\n    7\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/a.rs"),
        "pub fn a() -> i32 {\n    shared_helper() + 1\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/b.rs"),
        "pub fn b() -> i32 {\n    shared_helper() + 2\n}\n",
    )
    .unwrap();
    let eng = engine(&root, false);

    let map = eng.repo_map(8000);
    assert!(
        map.starts_with("\u{27E6}vyer/repo-map v1\u{27E7}"),
        "repo-map header: {map}"
    );
    // util.rs should be the #1 ranked line.
    let first_file_line = map.lines().nth(1).unwrap_or("");
    assert!(
        first_file_line.contains("src/util.rs"),
        "most-depended-upon file should rank first: {map}"
    );
    assert!(
        first_file_line.contains("shared_helper"),
        "top symbols should be listed"
    );
}

// ---- SCRY-031: post-apply verify hook ---------------------------------------

fn engine_with_verify(root: &Path, cmd: &[&str]) -> Engine {
    let mut cfg = EngineConfig::new(root.to_path_buf());
    cfg.allow_writes = true;
    cfg.verify_cmd = Some(cmd.iter().map(|s| s.to_string()).collect());
    Engine::new(cfg).unwrap()
}

#[test]
fn verify_hook_reports_pass() {
    let (_d, root) = fixture();
    let eng = engine_with_verify(&root, &["true"]); // a command that exits 0
    let out = eng
        .code_apply(&one_edit(
            "src/auth/login.rs#login",
            None,
            None,
            Some("pub fn login(u: &str) -> bool {\n    !u.is_empty()\n}"),
        ))
        .unwrap();
    assert!(
        out.contains("verify(true)=ok"),
        "missing pass report: {out}"
    );
}

#[test]
fn verify_hook_reports_failure() {
    let (_d, root) = fixture();
    let eng = engine_with_verify(&root, &["false"]); // exits non-zero
    let out = eng
        .code_apply(&one_edit(
            "src/auth/login.rs#login",
            None,
            None,
            Some("pub fn login(u: &str) -> bool {\n    !u.is_empty()\n}"),
        ))
        .unwrap();
    assert!(
        out.contains("verify(false)=FAILED"),
        "missing fail report: {out}"
    );
    // SCRY-055: a failed verify must tell the agent the edit IS written and that
    // undo:1 can revert it (verify runs post-apply and doesn't roll back).
    assert!(
        out.contains("undo:1") && out.contains("IS written"),
        "failed verify should guide the agent to undo: {out}"
    );
}

#[test]
fn verify_hook_skipped_on_dry_run() {
    let (_d, root) = fixture();
    let eng = engine_with_verify(&root, &["true"]);
    let req = ApplyRequest {
        edits: vec![Edit {
            locator: "src/auth/login.rs#login".into(),
            new_body: Some("pub fn login() -> bool { true }".into()),
            ..Default::default()
        }],
        dry_run: true,
        undo: None,
    };
    let out = eng.code_apply(&req).unwrap();
    assert!(
        !out.contains("verify("),
        "verify must not run on dry_run: {out}"
    );
}

#[test]
fn status_resource_reports_tiers() {
    let (_d, root) = fixture();
    let eng = engine(&root, true);
    let s = eng.status();
    assert!(s.contains("parser=tree-sitter"));
    assert!(s.contains("writes=enabled"));
    assert!(s.contains("semantic=lexical-subword"));
    assert!(s.contains("ast=tree-sitter-query"));
}

#[test]
fn jsonrpc_resources_list_and_read() {
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));

    let list = jsonrpc::handle(
        &eng,
        &json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
    )
    .unwrap();
    let uris: Vec<&str> = list["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"vyer://repo-map"));
    assert!(uris.contains(&"vyer://status"));

    let read = jsonrpc::handle(
        &eng,
        &json!({"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"vyer://status"}}),
    )
    .unwrap();
    let text = read["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains("vyer/status"));

    // unknown resource is an actionable error, not a panic.
    let bad = jsonrpc::handle(
        &eng,
        &json!({"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"vyer://nope"}}),
    )
    .unwrap();
    assert!(bad["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown resource"));
}

// ---- HTTP transport: loopback-only + bearer token --------------------------

#[test]
fn http_refuses_non_loopback_bind() {
    let err = http::bind("0.0.0.0:0".parse().unwrap()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn http_requires_token_and_serves_round_trip() {
    let (_d, root) = fixture();
    let eng = Arc::new(engine(&root, false));
    let listener = http::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = listener.local_addr().unwrap();
    let token = "s3cr3t-token".to_string();
    std::thread::spawn(move || {
        let _ = http::serve(listener, eng, token);
    });

    // 1) no token => 401, no body
    let (status, _) = http_post(
        addr,
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
    );
    assert_eq!(status, 401, "missing token must be rejected");

    // 2) wrong token => 401
    let (status, _) = http_post(
        addr,
        Some("nope"),
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
    );
    assert_eq!(status, 401, "wrong token must be rejected");

    // 3) correct token => 200 + real tools/call round-trip
    let (status, body) = http_post(
        addr,
        Some("s3cr3t-token"),
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"code","arguments":{"queries":[{"q":"validate_token"}]}}}"#,
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], 7);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("source=UNTRUSTED"),
        "round-trip envelope expected: {text}"
    );
}

/// Minimal HTTP POST helper for the test (no client dependency).
fn http_post(addr: std::net::SocketAddr, token: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}
