//! The transport-independent engine: everything the MCP layer does, minus the
//! protocol. Keeping it free of `rmcp`/`tokio` means the whole product loop —
//! index, search, fuse, pack, order, apply, freshness, security — is exercised
//! by ordinary synchronous unit/integration tests, with no client to stand up.
//!
//! Concurrency model (Rule §8): queries are read-mostly; the only `&mut` is a
//! write (apply). We hold the warm core behind a `Mutex` and clone small results
//! out, so a reader never observes a half-applied file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use vyer_core::{budget, fusion, locator::Locator, ordering, output, sandbox};
use vyer_incr::Db;

use crate::apply;
use crate::lexical;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Typed request/response surface (Rule §3: typed params only, no shell).
// Shared verbatim by the rmcp layer (it derives JsonSchema on top of these).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Query {
    /// Text / identifier / AST-ish pattern / natural language (per `mode`).
    /// Optional when `path` is set (a whole-file read needs no query).
    /// Lexical matching is **smart-case**: an all-lowercase `q` matches any case
    /// (`engine` finds `Engine`); add an uppercase letter to force exact case
    /// (`Engine` matches only `Engine`).
    #[serde(default)]
    pub q: String,
    /// Read-by-path: when set, return the whole file at this repo-relative path
    /// (line-numbered, budget-capped) with no search — the `Read` replacement.
    /// `detail` picks the view: `full` (the file), `outline` (symbol signatures),
    /// `locate` (a one-line summary).
    #[serde(default)]
    pub path: Option<String>,
    /// `auto` | `lexical` | `structural` | `graph` | `semantic`.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// `locate` | `outline` | `snippet` | `full` | `refs` | `impact` | `context`
    /// | `count` | `tree` | `diff` | `ast`. `ast` (with `path`) dumps the file's
    /// tree-sitter node-kinds + field labels so you can author a `mode=ast` query;
    /// scope it to one construct with `q`=a symbol name (preferred) or `lines`.
    /// `count` is grep -c and also accepts a boolean
    /// (all_of/any_of/none_of). `diff` returns every edit made to the repo this
    /// session (built from `code_apply` history; `q`/`path_scope` filter it).
    /// `outline` with no `q` and no `path` returns a whole-subtree symbol map
    /// (scope it with `path_scope` / `lang`) — orient on a directory in one call.
    #[serde(default = "default_detail")]
    pub detail: String,
    /// Glob filters (e.g. `src/auth/**`). Empty = whole repo. A `!`-prefixed glob
    /// is an EXCLUSION (`["src/**", "!**/tests/**"]` = src but not tests); with
    /// only exclusions, everything not excluded is in scope.
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// Restrict to a language: `rust` | `python` | `js` | `ts` | `go` | `dart`
    /// (and `java`/`ruby`/`swift`/`kotlin`/`c`/`cpp`/`cs`/`php`; aliases like `rs`,
    /// `py` work). Accepts a comma-separated list for polyglot repos: `ts,js`.
    #[serde(default)]
    pub lang: Option<String>,
    /// Line-range selector for a read-by-path (`path`) — the head/tail/sed -n
    /// replacement. 1-based, inclusive: `40-80` (range), `40` (one line),
    /// `40-` (to EOF), `-80` (first 80 = head), `~20` (last 20 = tail). Resolved
    /// against the warm-core line-offset index, so only the requested bytes are
    /// copied (no full-file line scan). Ignored when `path` is unset.
    #[serde(default)]
    pub lines: Option<String>,
    /// Boolean lexical refinement (line-level, composes with `q` and each other).
    /// A line is kept only if it contains **every** `all_of` literal (AND),
    /// **at least one** `any_of` literal when that list is non-empty (OR), and
    /// **none** of the `none_of` literals (NOT). Evaluated in a single
    /// Aho-Corasick pass with the candidate files pruned by the inverted index
    /// (rarest term first). Up to 64 operands total. `q` may be empty for a
    /// pure-boolean query.
    #[serde(default)]
    pub all_of: Vec<String>,
    #[serde(default)]
    pub any_of: Vec<String>,
    #[serde(default)]
    pub none_of: Vec<String>,
    /// Max candidates considered per query.
    #[serde(default = "default_k")]
    pub k: usize,
}
fn default_mode() -> String {
    "auto".into()
}
fn default_detail() -> String {
    "snippet".into()
}
fn default_k() -> usize {
    8
}

/// Request for the `code` tool. ACCEPTS MORE THAN THIS SCHEMA SHOWS (SCRY-128):
/// besides the canonical `{queries:[…]}`, you may send a SINGLE query's fields at
/// the top level (`{"q":"foo","detail":"snippet"}`) or a bare search string. For
/// the full, authoritative capability reference — every mode, detail, and a worked
/// example — call `code` with `{"detail":"help"}`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CodeRequest {
    pub queries: Vec<Query>,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
    /// Drop spans already returned earlier this session (iterative search).
    #[serde(default)]
    pub exclude_seen: bool,
}
fn default_budget() -> usize {
    8000
}

/// Build a [`Query`] from a JSON value that is EITHER a full query object OR a
/// bare search string (SCRY-128 sugar). A bare string is the single most common
/// thing an agent reaches for first, and `queries:["validateToken"]` used to be
/// rejected with a raw serde `invalid type: string, expected struct Query`.
fn query_from_value(v: serde_json::Value) -> Result<Query, serde_json::Error> {
    match v {
        serde_json::Value::String(s) => serde_json::from_value(serde_json::json!({ "q": s })),
        other => serde_json::from_value(other),
    }
}

// SCRY-128: hand-written so `code` accepts the shape an agent guesses first, not
// only the verbose canonical one. Three accepted forms, all wrapped to the
// canonical `{queries:[…]}` internally:
//   • {"q":"foo", "detail":"snippet"}   ← single-query SUGAR (90% case)
//   • {"queries":[{…}, "bareString"]}   ← batch (items may be strings)
//   • "foo"                              ← a bare search string
// Errors are tool-authored (they say what to send), never a raw serde dump.
impl<'de> Deserialize<'de> for CodeRequest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(_) => {
                let q = query_from_value(v).map_err(Error::custom)?;
                Ok(CodeRequest {
                    queries: vec![q],
                    budget_tokens: default_budget(),
                    exclude_seen: false,
                })
            }
            serde_json::Value::Object(map) => {
                let budget_tokens = map
                    .get("budget_tokens")
                    .and_then(|x| x.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or_else(default_budget);
                let exclude_seen = map
                    .get("exclude_seen")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                if let Some(qs) = map.get("queries") {
                    let arr = qs.as_array().ok_or_else(|| {
                        Error::custom(
                            "`queries` must be an array of query objects (or bare strings); \
                             for ONE query you can instead put the fields at the top level, e.g. {\"q\":\"foo\"}",
                        )
                    })?;
                    let mut queries = Vec::with_capacity(arr.len());
                    for item in arr {
                        let q = query_from_value(item.clone()).map_err(|e| {
                            Error::custom(format!(
                                "invalid item in `queries`: {e}. Each item is {{\"q\":\"…\",\"detail\":\"snippet\"}} or a bare string"
                            ))
                        })?;
                        queries.push(q);
                    }
                    Ok(CodeRequest {
                        queries,
                        budget_tokens,
                        exclude_seen,
                    })
                } else {
                    // single-query sugar: the whole object IS one query.
                    let q = query_from_value(serde_json::Value::Object(map)).map_err(|e| {
                        Error::custom(format!(
                            "invalid `code` arguments: {e}. Send {{\"q\":\"…\"}} for one query or {{\"queries\":[…]}} for a batch — or {{\"detail\":\"help\"}} for the full schema + examples"
                        ))
                    })?;
                    Ok(CodeRequest {
                        queries: vec![q],
                        budget_tokens,
                        exclude_seen,
                    })
                }
            }
            _ => Err(Error::custom(
                "`code` expects {\"q\":\"…\"} (one query), {\"queries\":[…]} (a batch), or a bare search string. Send {\"detail\":\"help\"} for the full schema + examples",
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Edit {
    /// `PATH#SYMBOL[@Lstart-end]` — the node to replace. Also accepts authoring
    /// directives: `PATH#@end`, `PATH#@after:SYMBOL`, `PATH#@before:SYMBOL`
    /// (insert a new symbol), `PATH#@into:CONTAINER[@Lstart-end]` (add a member
    /// inside a class/impl/struct, before its closing brace), and `PATH#@new`
    /// (create a new file). For an
    /// anchored edit, `PATH` alone scopes to the whole file (module-level edits),
    /// or `PATH#SYMBOL` scopes within that symbol.
    /// Defaulted (SCRY-128) only so a missing locator is reported as a tool-authored
    /// error that explains the format, not a raw serde `missing field 'locator'`.
    /// It is still REQUIRED for every edit op (validated in `code_apply`).
    #[serde(default)]
    pub locator: String,
    /// Anchored edit (SCRY-004): replace this exact (unique-in-scope) text with
    /// `replace`. Cheaper and safer than resending a whole body; with a bare
    /// `PATH` locator it can edit imports / top-level constants (SCRY-002).
    #[serde(default)]
    pub anchor: Option<String>,
    /// The replacement text for `anchor`.
    #[serde(default)]
    pub replace: Option<String>,
    /// Deterministic path: the full replacement text for the symbol's node
    /// (replace), or the body to insert (with an `@after:`/`@before:`/`@end`/
    /// `@new` locator).
    #[serde(default)]
    pub new_body: Option<String>,
    /// Fallback path: a lazy/sketch edit for the fast-apply model. (Accepted and
    /// gated; the model call is a Phase-6 sidecar — until then it is rejected
    /// with an actionable hint rather than silently mis-applied.)
    #[serde(default)]
    pub lazy_edit: Option<String>,
    /// Symbol-aware repo-wide rename (SCRY-027). With locator `PATH#SYMBOL`,
    /// renames the definition AND every whole-word reference across the repo to
    /// this new name — validated per file (each must still parse) and committed
    /// all-or-nothing. The cross-file refactor native tools can't do safely.
    #[serde(default)]
    pub rename: Option<String>,
    /// Move the symbol named by locator `PATH#SYMBOL` to this destination file
    /// (SP-5): cut it from the source, append it to the destination (creating it
    /// if needed). Both files are re-parsed and committed all-or-nothing.
    #[serde(default)]
    pub move_to: Option<String>,
    /// SCRY-046: scope a whole-word rename to ONE symbol. With `word: true`,
    /// `anchor`+`replace` rename EVERY whole-word occurrence of `anchor` within
    /// the locator `PATH#SYMBOL`'s body (or the whole file for a bare `PATH`) to
    /// `replace` — the safe LOCAL-variable rename that repo-wide `rename` can't do
    /// and a unique `anchor` won't (it forbids multiple occurrences). Re-parse-
    /// validated; rejects if the result doesn't parse.
    #[serde(default)]
    pub word: bool,
    /// SCRY-105: scope a repo-wide `rename` to files matching these globs (e.g.
    /// `["packages/auth/**"]`). In a MONOREPO a symbol name (`handler`, `index`,
    /// `config`) often recurs across packages as DISTINCT symbols, so a repo-wide
    /// rename over-renames; this confines it to one package precisely (still
    /// symbol-aware, unlike a text bulk-replace). Empty = repo-wide (the default).
    /// Same glob syntax as `code`'s `path_scope`; a `!`-prefixed entry excludes.
    #[serde(default)]
    pub path_scope: Vec<String>,
    /// SCRY-134 (guiding master): override a safety REFUSAL — e.g. deleting a
    /// symbol that still has references. Default false: vyer refuses the unsafe
    /// edit and tells you why (the reference sites), so a slip can't silently
    /// break callers. Set `force:true` only after you've seen and accepted the
    /// blast radius.
    #[serde(default)]
    pub force: bool,
}

/// Request for the `code_apply` tool. ACCEPTS MORE THAN THIS SCHEMA SHOWS
/// (SCRY-128): besides the canonical `{edits:[…]}`, you may send a SINGLE edit's
/// fields at the top level (`{"locator":"src/x.rs#foo","new_body":"…"}`) or
/// `{"undo":N}`. NO prior Read is needed and file bytes never enter your context.
/// For every op + a worked example, call the `code` tool with `{"detail":"help"}`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ApplyRequest {
    #[serde(default)]
    pub edits: Vec<Edit>,
    /// Validate + show the diff without writing.
    #[serde(default)]
    pub dry_run: bool,
    /// SP-6 undo: revert the last N successful `code_apply` batches (restores the
    /// files exactly as they were). When set, `edits` is ignored.
    #[serde(default)]
    pub undo: Option<usize>,
    /// SCRY-140 code_run: execute an OPERATOR-allowlisted task by NAME (e.g.
    /// `{"run":"test"}`) and return structured diagnostics. Gated by `--allow-run`.
    /// The request supplies only the task name — never a command/args (Rule §3).
    /// When set, `edits` is ignored.
    #[serde(default)]
    pub run: Option<String>,
}

// SCRY-128: like `code`, accept the single-edit shape an agent guesses first —
// `code_apply({"locator":"…","new_body":"…"})` — wrapped to `{edits:[…]}`. The
// canonical batch form and the `{undo:N}` form are unchanged.
impl<'de> Deserialize<'de> for ApplyRequest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        let map = match v {
            serde_json::Value::Object(m) => m,
            _ => {
                return Err(Error::custom(
                    "`code_apply` expects {\"edits\":[…]} (a batch), {\"locator\":\"…\",…} (one edit), or {\"undo\":N}",
                ))
            }
        };
        let dry_run = map
            .get("dry_run")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let undo = map.get("undo").and_then(|x| x.as_u64()).map(|n| n as usize);
        let run = map.get("run").and_then(|x| x.as_str()).map(str::to_string);
        if let Some(es) = map.get("edits") {
            let arr = es.as_array().ok_or_else(|| {
                Error::custom(
                    "`edits` must be an array; for ONE edit put its fields at the top level, e.g. {\"locator\":\"src/x.rs#foo\",\"new_body\":\"…\"}",
                )
            })?;
            let mut edits = Vec::with_capacity(arr.len());
            for item in arr {
                let e: Edit = serde_json::from_value(item.clone())
                    .map_err(|err| Error::custom(format!("invalid item in `edits`: {err}")))?;
                edits.push(e);
            }
            Ok(ApplyRequest {
                edits,
                dry_run,
                undo,
                run,
            })
        } else if undo.is_some() || run.is_some() {
            Ok(ApplyRequest {
                edits: Vec::new(),
                dry_run,
                undo,
                run,
            })
        } else {
            // single-edit sugar: the whole object IS one edit.
            let e: Edit = serde_json::from_value(serde_json::Value::Object(map)).map_err(|err| {
                Error::custom(format!(
                    "invalid `code_apply` arguments: {err}. Send one edit like {{\"locator\":\"src/x.rs#foo\",\"new_body\":\"…\"}}, a batch {{\"edits\":[…]}}, {{\"run\":\"test\"}}, or {{\"undo\":N}} — call the `code` tool with {{\"detail\":\"help\"}} for every op + a worked example"
                ))
            })?;
            Ok(ApplyRequest {
                edits: vec![e],
                dry_run,
                undo: None,
                run: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------

pub struct EngineConfig {
    pub root: PathBuf,
    pub allow_writes: bool,
    pub max_file_bytes: usize,
    /// Optional file the audit log is appended to (always kept in memory too).
    pub audit_path: Option<PathBuf>,
    /// SCRY-031 post-apply verify: an OPERATOR-configured command (argv) run in
    /// the repo root after a successful write batch — e.g. `["cargo", "check"]`,
    /// `["pytest", "-q"]`, `["tsc", "--noEmit"]`. Its pass/fail is reported inline
    /// so an agent learns whether an edit *compiles*, not just *parses*. It is set
    /// at launch (like `--allow-writes`), NEVER from a request — so this is not a
    /// generic command-execution surface (Rule §3 holds).
    pub verify_cmd: Option<Vec<String>>,
    /// SCRY-140 `code_run`: OPERATOR-allowlisted task name → argv. The agent's
    /// request selects a task by NAME (e.g. `{"run":"test"}`); it can never supply
    /// a command string or args. This keeps Rule §3 (typed ops only, no shell
    /// passthrough) — same trust model as `verify_cmd`, just request-triggered —
    /// and folds into `code_apply` so the tool count stays at two (Rule §1).
    pub run_tasks: std::collections::BTreeMap<String, Vec<String>>,
    /// Gate for `code_run` (distinct effect class from writes). Off by default:
    /// the agent cannot execute anything unless the operator passed `--allow-run`.
    pub allow_run: bool,
}

impl EngineConfig {
    pub fn new(root: PathBuf) -> Self {
        EngineConfig {
            root,
            allow_writes: false,
            max_file_bytes: 1 << 20,
            audit_path: None,
            verify_cmd: None,
            run_tasks: std::collections::BTreeMap::new(),
            allow_run: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub unix_secs: u64,
    pub tool: String,
    pub summary: String,
}

pub struct Engine {
    config: EngineConfig,
    db: Mutex<Db>,
    seen: Mutex<HashSet<String>>,
    audit: Mutex<Vec<AuditEntry>>,
    /// Cached token→files postings for candidate pruning, keyed by the warm
    /// core's revision so it rebuilds only when something actually changed.
    /// Turns lexical search from "scan every file" into "scan the few files that
    /// contain the token" — the inverted-index win, kept dependency-free.
    token_index: Mutex<Option<(u64, TokenIndex)>>,
    /// Cached semantic subword-TF-IDF index, same revision-keyed pattern as
    /// `token_index` — so a low-confidence `mode=auto` escalation (or `semantic`)
    /// doesn't rebuild it every query.
    semantic_index: Mutex<Option<(u64, SemanticIndex)>>,
    /// SP-6 undo stack: each successful `code_apply` batch pushes the pre-batch
    /// text of every file it touched (`None` = the file didn't exist). `undo`
    /// pops and restores — safe exploration for an agent.
    history: Mutex<Vec<EditBatch>>,
}

/// One undoable batch: the pre-edit text of each file it touched
/// (`None` = the file did not exist before).
type EditBatch = Vec<(String, Option<String>)>;

/// SCRY-071: the undo history is bounded so a long-lived daemon's memory stays
/// flat across a heavy editing session (each batch retains pre-edit file text).
const MAX_UNDO_BATCHES: usize = 256;

/// SCRY-072: the in-memory audit log is bounded for the same reason (the `--audit`
/// file, if configured, retains the full append-only record).
const MAX_AUDIT_ENTRIES: usize = 10_000;

/// SCRY-073: the `exclude_seen` paging set is bounded; on overflow it resets a
/// fresh paging cycle rather than grow unbounded across a long session.
const MAX_SEEN_SPANS: usize = 50_000;

/// token (lowercased identifier) → files that contain it.
/// SCRY-079: the inverted token index, maintained INCREMENTALLY. `postings` is the
/// token→files map consumers read; `file_tokens` records each file's content hash
/// and the tokens it contributed, so a write re-tokenizes only the changed files
/// instead of rebuilding the whole repo (a full rebuild was ~950ms on a 50k-file
/// repo — far past the §7 "<50ms incremental" SLO; the update is now ~ms).
#[derive(Default)]
struct TokenIndex {
    postings: HashMap<String, Vec<String>>,
    file_tokens: HashMap<String, (u64, Vec<String>)>,
}

struct Cand {
    path: String,
    symbol: Option<String>,
    start: u32,
    end: u32,
    /// The line a LEXICAL match actually landed on (SCRY-131), used to WINDOW a
    /// `snippet` around the hit instead of dumping the whole enclosing symbol.
    /// `None` for structural/semantic/graph candidates (no single hit line) —
    /// those window from the symbol's start.
    hit: Option<u32>,
}

impl Engine {
    /// Build an engine over `root`, indexing every text file the repo's ignore
    /// rules permit into the warm core. Indexing is the only place we read disk
    /// in bulk; everything after reads the in-memory core.
    pub fn new(config: EngineConfig) -> std::io::Result<Self> {
        let mut raw = Db::new();
        // Install the real tree-sitter parser at the incremental core's parser
        // hook (Phase 2). Everything downstream — symbols, outline, snippet
        // boundaries, the apply anchor — now uses AST-accurate node spans.
        raw.set_parser(vyer_index::tree_sitter_parser());
        let db = Mutex::new(raw);
        let engine = Engine {
            config,
            db,
            seen: Mutex::new(HashSet::new()),
            audit: Mutex::new(Vec::new()),
            token_index: Mutex::new(None),
            semantic_index: Mutex::new(None),
            history: Mutex::new(Vec::new()),
        };
        engine.index_repo()?;
        Ok(engine)
    }

    fn index_repo(&self) -> std::io::Result<()> {
        let root = &self.config.root;
        let mut db = self.db.lock().unwrap();
        for result in ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            // Prune well-known build/vendor dirs even when there is no .gitignore
            // (e.g. a bare checkout): never index `target/` or `node_modules/`.
            .filter_entry(|e| !is_skippable_dir(e.file_name().to_str().unwrap_or("")))
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue, // unreadable entry: skip, never crash indexing
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let meta = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() as usize > self.config.max_file_bytes {
                continue; // huge file: skip (degrade, don't choke)
            }
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if bytes.contains(&0) {
                continue; // binary heuristic
            }
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(_) => continue, // non-UTF-8: skip
            };
            if let Some(rel) = rel_path(root, path) {
                db.set_text(&rel, &text);
            }
        }

        // SCRY-114: reconcile DELETIONS. The walk above ADDS/UPDATES every file
        // present on disk, but a file removed out-of-band (shell `rm`, `git
        // checkout`) would otherwise linger in the warm core until restart. Drop
        // every indexed file whose path no longer exists. We test on-disk
        // existence (not walk membership) so a gitignored-but-present file added
        // via `code_apply` is NOT purged.
        let stale: Vec<String> = db
            .files()
            .into_iter()
            .filter(|rel| !root.join(rel).exists())
            .collect();
        for rel in stale {
            db.remove_text(&rel);
        }
        Ok(())
    }

    pub fn audit_log(&self) -> Vec<AuditEntry> {
        self.audit.lock().unwrap().clone()
    }

    /// Re-read one file from disk into the warm core. This is how edits made
    /// *outside* `code_apply` (an external editor, a `git checkout`, the FS
    /// watcher) become query-ready. `set_text` is a no-op when the content is
    /// unchanged, so spurious events are cheap. Returns whether the file was
    /// (re)indexed.
    pub fn reindex_path(&self, rel: &str) -> bool {
        let abs = self.config.root.join(rel);
        let meta = match abs.metadata() {
            Ok(m) => m,
            Err(_) => return false,
        };
        if !meta.is_file() || meta.len() as usize > self.config.max_file_bytes {
            return false;
        }
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => return false,
        };
        if bytes.contains(&0) {
            return false;
        }
        if let Ok(text) = String::from_utf8(bytes) {
            self.db.lock().unwrap().set_text(rel, &text);
            self.record("reindex", rel.to_string());
            true
        } else {
            false
        }
    }

    /// Re-walk the repo and re-index everything (e.g. after a branch switch).
    pub fn reindex_all(&self) -> std::io::Result<()> {
        self.index_repo()
    }

    /// Map an absolute path under the root to its repo-relative key, if inside.
    pub fn rel_of(&self, abs: &std::path::Path) -> Option<String> {
        rel_path(&self.config.root, abs)
    }

    pub fn root(&self) -> &std::path::Path {
        &self.config.root
    }

    pub fn indexed_files(&self) -> Vec<String> {
        self.db.lock().unwrap().files()
    }

    fn record(&self, tool: &str, summary: String) {
        // SCRY-094: keep every audit entry to ONE tab-delimited line — a summary can
        // embed query/locator-derived text, so neutralize newlines/tabs/CR or a
        // crafted input could inject a FAKE audit line or break the TSV columns,
        // defeating the forensic value of the log (audit integrity, §9).
        let summary = summary.replace(['\n', '\r', '\t'], " ");
        let unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = AuditEntry {
            unix_secs,
            tool: tool.to_string(),
            summary,
        };
        if let Some(p) = &self.config.audit_path {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                let _ = writeln!(f, "{}\t{}\t{}", entry.unix_secs, entry.tool, entry.summary);
            }
        }
        let mut audit = self.audit.lock().unwrap();
        audit.push(entry);
        // SCRY-072: bound the in-memory audit so a long-lived daemon's memory stays
        // flat (the `--audit` FILE, if set, keeps the full append-only record).
        if audit.len() > MAX_AUDIT_ENTRIES {
            let excess = audit.len() - MAX_AUDIT_ENTRIES;
            audit.drain(0..excess);
        }
    }

    // ----- the `code` tool -------------------------------------------------

    pub fn code(&self, req: &CodeRequest) -> String {
        let db = self.db.lock().unwrap();
        let mut cands: HashMap<String, Cand> = HashMap::new();
        let mut spans: Vec<budget::Span> = Vec::new();
        let mut summary_q: Vec<String> = Vec::new();
        let mut low_confidence = false;
        // SCRY-058: an unrecognized `mode` silently falls back to auto; remember it
        // so the envelope can tell the agent its param was ignored (not honored).
        let mut unknown_mode: Option<String> = None;
        let mut unknown_detail: Option<String> = None;
        // SCRY-124 (#4): an unrecognized `lang` filters to 0 files silently; remember
        // it so the envelope warns instead of returning a bare empty result.
        let mut unknown_lang: Option<String> = None;
        // SCRY-024: per-query attribution. We record `spans.len()` at the start of
        // each query iteration (and once more after the loop); the delta between
        // consecutive marks is that query's hit count. Lets a batched caller see
        // which of its queries matched and which came back empty — the fused span
        // list alone hides that.
        let mut marks: Vec<usize> = Vec::new();

        // SCRY-023: snapshot already-seen ids up front so `exclude_seen` filters
        // *before* the per-query top-k cut. Filtering after truncation (the old
        // behaviour) dead-ended paging: once the top-k were seen, a repeat call
        // returned empty even though lower-ranked unseen matches existed.
        let seen_snapshot: HashSet<String> = if req.exclude_seen {
            self.seen.lock().unwrap().iter().cloned().collect()
        } else {
            HashSet::new()
        };

        // SCRY-142: per-query FAIR-SHARE budget. In a BATCH, one query that matches a
        // lot (and returns big spans) used to consume the whole token pool and starve
        // its siblings — so batching, which should help, hurt. Cap each query's
        // contribution to ~budget/N (with a floor, and always its top span). A single
        // query (N=1) gets the full budget, so non-batch behavior is unchanged.
        let per_query_budget = (req.budget_tokens / req.queries.len().max(1)).max(600);

        for query in &req.queries {
            summary_q.push(format!("{}({}/{})", query.q, query.mode, query.detail));
            marks.push(spans.len());
            if let Some(l) = &query.lang {
                if !l.trim().is_empty() && lang_extensions(l).is_empty() {
                    unknown_lang = Some(l.clone());
                }
            }

            // SCRY-132: `detail=help` — the live, self-describing capability sheet.
            // Every agent report cited schema drift (prose vs the real params) as the
            // #1 failure: both tools rejected the documented call shape. This makes
            // the SCHEMA the source of truth an agent can fetch in one call instead of
            // guessing against rotting prose — modes, details, the apply shape, and a
            // valid example per surface. Checked first so it's robust to any mode.
            if query.detail == "help" {
                spans.push(budget::Span {
                    id: "vyer/help".into(),
                    text: CODE_HELP.to_string(),
                    score: 1.0,
                });
                continue;
            }

            // SUPERPOWER (SCRY-116): `mode=diagnose` — paste a compiler/test/stack-trace blob into
            // `q` → the exact code it references (enclosing symbol + a window with the failing line
            // marked `>>`), best-at-the-edges, root cause first. Checked FIRST so it's robust to any
            // `detail`. Closes the run → error → fix loop; the agent stops hand-grepping file:line.
            if query.mode == "diagnose" {
                spans.extend(self.diagnose_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SCRY-118): `detail=import` — resolve `q` (a symbol) to its defining file
            // via the index, and (with `path` = the file to import INTO) construct the exact import
            // statement for that file's language. The reliable half of "add-import": vyer KNOWS
            // where the symbol lives; the agent inserts the returned line. (Auto-insertion is the
            // LSP-gated tier — see docs/REFACTOR-AND-DEPS.md.)
            if query.detail == "import" {
                spans.extend(self.import_spans(&db, query));
                continue;
            }

            // `refs` (graph) is its own path: resolve definition(s) + approximate
            // cross-file references. We have no LSP yet, so the resolution is a
            // tree-sitter/lexical approximation and is HONESTLY reported as
            // `graph=partial` (Rule §8: report the tier, never fake `full`).
            // SCRY-085: `mode=graph` defaults to `refs`, but only when the detail
            // isn't a MORE-specific graph detail — else `mode=graph detail=context`
            // (a natural pairing) was silently downgraded to refs, skipping the
            // context/impact checks below. The detail drives the graph operation.
            if query.detail == "refs"
                || (query.mode == "graph" && query.detail != "context" && query.detail != "impact")
            {
                spans.extend(self.refs_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-3): `detail=impact` — the transitive blast radius of
            // changing this symbol (who calls it, who calls them, …) in one call.
            // The "if I change X, what breaks?" an agent can't get from one grep.
            if query.detail == "impact" {
                spans.extend(self.impact_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-9): `detail=context` — one call returns everything an
            // agent needs to understand a symbol: its definition (full), what it
            // calls (callees), what calls it (callers), and its tests. Replaces
            // the 4–8 calls an agent makes to assemble this by hand.
            if query.detail == "context" {
                spans.extend(self.context_spans(&db, query));
                continue;
            }

            // SUPERPOWER: `detail=count` — grep -c / wc -l, but candidate-pruned
            // via the inverted index and counted in one SIMD pass (memmem) with
            // no hit allocation. Reports BOTH matching lines and total matches.
            if query.detail == "count" {
                spans.extend(self.count_spans(&db, query));
                continue;
            }

            // SUPERPOWER: `detail=tree` — ls / find / tree over the resident,
            // ignore-filtered, already-sorted file set. No readdir/stat syscalls.
            if query.detail == "tree" {
                spans.extend(self.tree_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-10): `detail=diff` — every change made this session,
            // in one call. Reuses the SP-6 history snapshot stack: for each file an
            // agent touched via `code_apply`, diff its session-original text against
            // the current warm-core text. Deterministic, in-process, no `git` — the
            // "what have I changed so far?" an agent otherwise reconstructs by hand.
            if query.detail == "diff" {
                spans.extend(self.diff_spans(&db, query));
                continue;
            }

            // SP-13: `detail=ast` dumps the tree-sitter node-kinds of a file
            // (`path` required) so an agent can author `mode=ast` S-expression
            // queries — the `dump_ast` affordance the no-match hints point at.
            if query.detail == "ast" {
                spans.extend(self.dump_ast_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-7): `mode=ast` — a real tree-sitter pattern query.
            if query.mode == "ast" {
                spans.extend(self.ast_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-7): `mode=semantic` — conceptual retrieval for the
            // "I don't know the exact name" case. Deterministic + zero-dependency:
            // subword-tokenize names/signatures (camelCase + snake_case), rank by
            // TF-IDF overlap with the query. Honestly labelled lexical-subword
            // (not neural embeddings), but it finds `validate_token` from "check
            // if a token is valid".
            if query.mode == "semantic" {
                spans.extend(self.semantic_spans(&db, query));
                continue;
            }

            // SUPERPOWER (SP-11): subtree outline. `detail=outline` with no `q`
            // and no `path` returns the symbol signatures of every file in scope
            // — the "lay of the land for this directory" an agent reads to orient,
            // in one call instead of N per-file reads. Scope with path_scope
            // (incl. `!` exclusions) and/or lang.
            if query.detail == "outline" && query.q.is_empty() && query.path.is_none() {
                spans.extend(self.outline_spans(&db, query));
                continue;
            }

            // Read-by-path (SCRY-003): the `Read` replacement. When `path` is
            // given, return the whole file (or its outline/summary) directly —
            // no query, no symbol name, no search.
            if let Some(p) = &query.path {
                spans.extend(self.read_path_spans(&db, query, p, req.budget_tokens));
                continue;
            }

            // SCRY-049: an empty `q` reaching the fused search (no path, no boolean,
            // and not one of the q-less detail modes handled above) has no search
            // term — lexical/structural would otherwise match EVERY line/symbol and
            // dump the whole repo (anti Rule §6). Contribute nothing instead.
            if query.q.trim().is_empty() && !has_bool(query) {
                continue;
            }

            let files = self.scoped_files(&db, query);
            // Prune the lexical scan to files that actually contain the query
            // token (inverted index); for a boolean query, intersect the AND
            // terms' postings (rarest first). Structural search reads cheap
            // symbol tables so it stays over the full scoped set.
            let lex_files = self.planned_lex_files(&db, query, &files);
            // SCRY-054: when paging (exclude_seen), build a POOL larger than the
            // displayed `k` so successive pages reveal NEW matches; the per-file
            // search would otherwise stop at `k` hits and paging would dead-end
            // once the first `k` are seen. Non-paging queries keep the tight `k`.
            let lex_pool = if req.exclude_seen {
                query.k.max(1).max(256)
            } else {
                query.k.max(1)
            };
            let lexical_ids = self.lexical_ids(&db, query, &lex_files, lex_pool, &mut cands);
            // SCRY-048: prune STRUCTURAL search too. A symbol whose name contains
            // a plain-identifier query lives in a file that contains that
            // identifier, so the lexical candidate set is a sound superset — read
            // only those files' symbol tables instead of every scoped file. Non
            // plain-ident queries keep the full scoped set (correctness first).
            let struct_files: &[String] = if is_plain_ident(&query.q) {
                &lex_files
            } else {
                &files
            };
            let structural_ids = self.structural_ids(&db, query, struct_files, &mut cands);

            // By here graph/semantic/ast modes have already taken their own paths
            // (continue), so only auto/lexical/structural are valid — anything else
            // is a typo that fell back to auto (SCRY-058).
            if !matches!(query.mode.as_str(), "auto" | "lexical" | "structural") {
                unknown_mode = Some(query.mode.clone());
            }
            // Likewise: the q-less/branch details (refs/impact/context/count/tree/
            // diff/ast/outline) already took their paths, so only these reach the
            // fused search — anything else is a typo that fell back to snippet.
            if !matches!(
                query.detail.as_str(),
                "locate" | "outline" | "snippet" | "full"
            ) {
                unknown_detail = Some(query.detail.clone());
            }
            let base_lists: Vec<(f64, Vec<String>)> = match query.mode.as_str() {
                "lexical" => vec![(1.0, lexical_ids.clone())],
                "structural" => vec![(1.0, structural_ids.clone())],
                _ => vec![(0.8, structural_ids.clone()), (0.8, lexical_ids.clone())],
            };
            let mut fused = fusion::rrf_weighted(&base_lists, 60.0);

            // Confidence gate + Rule §5 escalation (auto): when the cheap modalities
            // are ambiguous (top-2 fused scores within 10%) OR return nothing,
            // escalate to the semantic (subword-TF-IDF) modality and re-fuse it in
            // at weight 0.2. This is the reranker the gate used to only *flag* — it
            // recovers the "I don't know the exact name" case lexical/structural
            // miss. Semantic is deterministic + zero-dependency, so `auto` stays
            // deterministic (Rule §9).
            if query.mode == "auto" {
                let close = fused.len() >= 2 && {
                    let top = fused[0].1.max(1e-9);
                    (fused[0].1 - fused[1].1) / top < 0.10
                };
                // SCRY-131: do NOT escalate to semantic when the query is an exact
                // symbol that EXISTS. An exact identifier (`_slotRow`) should
                // dominate, not be diluted by fuzzy cross-repo neighbors. The
                // "top-2 are close" signal fires spuriously for exact names because
                // raw RRF scores are tiny and bunched — without this guard `auto`
                // over-escalates on every literal identifier and buries the def.
                let exact_hit = is_plain_ident(&query.q) && {
                    let smart_exact = query.q.bytes().any(|b| b.is_ascii_uppercase());
                    cands.values().any(|c| {
                        c.symbol.as_deref().is_some_and(|s| {
                            if smart_exact {
                                s == query.q
                            } else {
                                s.eq_ignore_ascii_case(&query.q)
                            }
                        })
                    })
                };
                if (close || fused.is_empty()) && !exact_hit {
                    let sem = self.semantic_ids(&db, query, &files, &mut cands);
                    // SCRY-057: also surface a symbol the agent named in prose. An
                    // exact symbol-name word is a stronger signal than tf-idf
                    // overlap, so weight it above semantic (0.3 vs 0.2).
                    let mention = self.symbol_mention_ids(&db, query, &files, &mut cands);
                    if !sem.is_empty() || !mention.is_empty() {
                        let lists2 = vec![
                            (0.8, structural_ids),
                            (0.8, lexical_ids),
                            (0.3, mention),
                            (0.2, sem),
                        ];
                        fused = fusion::rrf_weighted(&lists2, 60.0);
                    }
                    low_confidence = close;
                }
            }

            let mut q_used = 0usize;
            let mut q_pushed = 0usize;
            for (id, score) in fused
                .into_iter()
                .filter(|(id, _)| !req.exclude_seen || !seen_snapshot.contains(id))
                .take(query.k.max(1))
            {
                if let Some(c) = cands.get(&id) {
                    if let Some(span) = self.expand(&db, query, &id, c, score) {
                        // SCRY-142: stop once this query has used its fair share — but
                        // always keep its top span so every query in a batch is heard.
                        let cost = budget::est_tokens(&span.text) + 8;
                        if q_pushed >= 1 && q_used + cost > per_query_budget {
                            break;
                        }
                        q_used += cost;
                        q_pushed += 1;
                        spans.push(span);
                    }
                }
            }
        }

        // Close the marks series: the final entry is the total span count, so
        // hits(query i) = marks[i+1] - marks[i].
        marks.push(spans.len());

        // SCRY-064: a batch with overlapping queries can match the SAME span more
        // than once; dedup by id (keeping the first/highest-ranked occurrence) so
        // the agent never pays tokens for a duplicate. Marks above stay raw
        // (per-query match counts); only the OUTPUT is deduped.
        {
            let mut seen_ids: HashSet<String> = HashSet::new();
            spans.retain(|s| seen_ids.insert(s.id.clone()));
        }

        // Record the now-returned ids so the *next* exclude_seen call pages past
        // them (the filtering itself already happened pre-truncation, above).
        if req.exclude_seen {
            let mut seen = self.seen.lock().unwrap();
            // SCRY-073: bound the paging set so a long session of exclude_seen calls
            // can't grow it without limit. If it ever gets huge, reset (a fresh
            // paging cycle) BEFORE re-adding this page, so the current results are
            // still excluded next time.
            if seen.len() > MAX_SEEN_SPANS {
                seen.clear();
            }
            for s in &spans {
                seen.insert(s.id.clone());
            }
        }

        // No hits anywhere → an actionable error envelope, not an empty result.
        if spans.is_empty() {
            self.record("code", format!("queries=[{}] hits=0", summary_q.join(", ")));
            // SCRY-127: distinguish "a positive path_scope filtered every file out"
            // from "the pattern is genuinely absent". Conflating them (always
            // PATTERN_NO_MATCH) sent agents debugging the wrong thing — the token
            // looked missing when really the scope excluded every candidate. Scoped
            // to POSITIVE entries only: exclusion-only over-filtering and lang
            // mismatches keep their existing tailored PATTERN_NO_MATCH hints.
            if let Some(q) = req.queries.iter().find(|q| {
                q.path_scope.iter().any(|g| !g.starts_with('!'))
                    && self.scoped_files(&db, q).is_empty()
            }) {
                return output::format_error("SCOPE_NO_MATCH", &scope_no_match_hint(q));
            }
            // SCRY-137: fuzzy "did you mean" recovery. AI agents mistype identifiers;
            // rather than dead-end on PATTERN_NO_MATCH and burn a round-trip, return
            // the nearest symbols by bounded edit distance so the agent self-corrects
            // in ONE call. Only here (the rare no-match path), only for a plain
            // identifier — exact search stays exact (the grep floor is untouched).
            if let Some(q) = req
                .queries
                .iter()
                .rfind(|q| is_plain_ident(&q.q) && q.q.trim().len() >= 3)
            {
                let near = self.fuzzy_symbol_matches(&db, q);
                if !near.is_empty() {
                    let mut fz = Vec::new();
                    for (f, s, dist) in near.into_iter().take(q.k.max(1)) {
                        let text = db.text(&f).unwrap_or_else(|| std::sync::Arc::from(""));
                        let id = make_id(&f, Some(&s.name), s.start, s.end, &text);
                        fz.push(budget::Span {
                            id,
                            text: format!(
                                "L{}-{} [{}] {}  (fuzzy: edit-distance {dist} from `{}`)",
                                s.start,
                                s.end,
                                s.kind,
                                s.signature.trim(),
                                q.q
                            ),
                            score: 1.0 - (dist as f64) / 10.0,
                        });
                    }
                    let ordered = ordering::lost_in_the_middle(fz);
                    let used: usize = ordered
                        .iter()
                        .map(|s| budget::est_tokens(&s.text) + 8)
                        .sum();
                    let mut env =
                        output::format_result(&ordered, req.budget_tokens, used, false, 0);
                    env.push_str(&output::note_line(&format!(
                        "no EXACT match for `{}` — these are the nearest symbols by edit distance (did you mean one of them?)",
                        q.q
                    )));
                    return env;
                }
            }
            let mut err = output::format_error("PATTERN_NO_MATCH", &no_match_hint(&req.queries));
            // SCRY-124 (#4): an unknown `lang` filter is the likely cause of an empty
            // result — surface it here too (the envelope notes below are skipped on
            // the no-match path).
            if let Some(l) = &unknown_lang {
                err.push_str(&output::note_line(&format!(
                    "unknown lang `{l}` — filter matched 0 files; known: rust|python|js|ts|go|dart|java|ruby|swift|kotlin|c|cpp|cs|php|yaml|json|toml|md|html|css|sh|xml (comma-separated ok)"
                )));
            }
            return err;
        }

        let total = spans.len();
        let (kept, truncated) = budget::pack(spans, req.budget_tokens, 8);
        let omitted = total.saturating_sub(kept.len());
        let mut ordered = ordering::lost_in_the_middle(kept);
        // SCRY-129: display the score RELATIVE to the top hit (0..1), not the raw
        // RRF magnitude. Raw RRF values are tiny (~0.003–0.016) and render via
        // `{:.2}` as 0.00/0.01 — meaningless to an agent, and a semantic-escalated
        // result (weight 0.2) showed ALL spans as score=0.00. Scaling by the max
        // makes the best span read 1.00 and conveys real within-set confidence.
        // Monotonic, so the pack/order decisions above are unaffected.
        {
            let max = ordered.iter().map(|s| s.score).fold(0.0_f64, f64::max);
            if max > 0.0 {
                for s in &mut ordered {
                    s.score /= max;
                }
            }
        }
        let used: usize = ordered
            .iter()
            .map(|s| budget::est_tokens(&s.text) + 8)
            .sum();
        let mut envelope =
            output::format_result(&ordered, req.budget_tokens, used, truncated, omitted);
        if low_confidence {
            envelope.push_str(&output::note_line(
                "auto confidence=low (top results were close); escalated to the semantic modality and re-fused — if results look off, try mode=structural or a more specific query",
            ));
        }
        if let Some(m) = &unknown_mode {
            envelope.push_str(&output::note_line(&format!(
                "unknown mode `{m}` — used `auto`; valid modes: auto|lexical|structural|graph|semantic|ast|diagnose"
            )));
        }
        if let Some(d) = &unknown_detail {
            envelope.push_str(&output::note_line(&format!(
                "unknown detail `{d}` — used `snippet`; valid: locate|outline|snippet|full|refs|impact|context|count|tree|diff|ast|import|help (call detail=help for the full schema + examples)"
            )));
        }
        if let Some(l) = &unknown_lang {
            envelope.push_str(&output::note_line(&format!(
                "unknown lang `{l}` — filter matched 0 files; known: rust|python|js|ts|go|dart|java|ruby|swift|kotlin|c|cpp|cs|php|yaml|json|toml|md|html|css|sh|xml (comma-separated ok)"
            )));
        }
        // SCRY-024: when more than one query is batched, attribute how many spans
        // each found so the agent can tell which matched and re-issue the empties.
        if req.queries.len() >= 2 && marks.len() == req.queries.len() + 1 {
            let attribution = req
                .queries
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    let hits = marks[i + 1].saturating_sub(marks[i]);
                    let label = if !q.q.is_empty() {
                        q.q.clone()
                    } else if let Some(p) = &q.path {
                        p.clone()
                    } else if has_bool(q) {
                        // SCRY-124 (NEW-D): label a boolean query by its TERMS, not
                        // its detail value.
                        let mut parts = Vec::new();
                        if !q.all_of.is_empty() {
                            parts.push(format!("all[{}]", q.all_of.join(",")));
                        }
                        if !q.any_of.is_empty() {
                            parts.push(format!("any[{}]", q.any_of.join(",")));
                        }
                        if !q.none_of.is_empty() {
                            parts.push(format!("not[{}]", q.none_of.join(",")));
                        }
                        parts.join(" ")
                    } else {
                        q.detail.clone()
                    };
                    format!("q{i} `{label}`→{hits}")
                })
                .collect::<Vec<_>>()
                .join("  ");
            envelope.push_str(&output::note_line(&format!(
                "per-query found: {attribution}"
            )));
        }

        self.record(
            "code",
            format!(
                "queries=[{}] hits={} truncated={}",
                summary_q.join(", "),
                ordered.len(),
                truncated
            ),
        );
        envelope
    }

    fn scoped_files(&self, db: &Db, q: &Query) -> Vec<String> {
        let lang_exts = q.lang.as_deref().map(lang_extensions);
        // SCRY-107: the Lang enums the filter names, so an EXTENSIONLESS shebang script
        // (SCRY-106, detected by content) is matched by `lang:` too — not just files
        // with a matching extension. Extensioned files (incl. `.h`-in-both-c/cpp) keep
        // the fast extension path; the db.lang check only rescues extensionless files.
        let lang_enums = q.lang.as_deref().map(lang_enums);
        db.files()
            .into_iter()
            .filter(|p| {
                if let Some(exts) = &lang_exts {
                    let ext_match = exts.iter().any(|e| p.ends_with(e));
                    let shebang_match = is_extensionless(p)
                        && lang_enums
                            .as_ref()
                            .is_some_and(|ls| ls.contains(&db.lang(p)));
                    if !ext_match && !shebang_match {
                        return false;
                    }
                }
                path_in_scope(p, &q.path_scope)
            })
            .collect()
    }

    /// Restrict the lexical scan to files that contain the query token, using
    /// the cached inverted index. For non-identifier (regex/multi-word) queries
    /// we can't prune, so we scan the full scoped set.
    fn pruned_lex_files(&self, db: &Db, q: &Query, scoped: &[String]) -> Vec<String> {
        if !is_plain_ident(&q.q) {
            // SCRY-047: a multi-word PURE-LITERAL phrase (no regex metachars) can
            // still be pruned — every match contains every token, so intersect
            // their postings instead of scanning every file. Regex/alternation
            // queries fall through to the full scoped scan (correctness first).
            if let Some(tokens) = literal_phrase_tokens(&q.q) {
                return self.and_candidate_files(db, &tokens, scoped);
            }
            // SCRY-066: a FLAT regex — prune by ALL its required literals (most
            // selective). SCRY-065: else a grouped/anchored regex with a required
            // literal PREFIX. Else full scan. All recall-preserving.
            let lits = regex_required_literals(&q.q);
            if !lits.is_empty() {
                return self.and_candidate_files(db, &lits, scoped);
            }
            if let Some(prefix) = regex_required_prefix(&q.q) {
                return self.and_candidate_files(db, &[prefix], scoped);
            }
            return scoped.to_vec();
        }
        let key = q.q.to_ascii_lowercase();
        let mut guard = self.token_index.lock().unwrap();
        ensure_token_index(&mut guard, db);
        let postings = &guard.as_ref().unwrap().1.postings;
        // SCRY-038: substring-aware candidate set (see `postings_substring`) so a
        // prefix/substring query (`valid` for `validate_token`) keeps full recall.
        let cand = postings_substring(postings, &key);
        if cand.is_empty() {
            return Vec::new(); // no identifier contains the needle → no lexical hits
        }
        scoped
            .iter()
            .filter(|f| cand.contains(f.as_str()))
            .cloned()
            .collect()
    }

    /// Candidate files for the lexical scan, with query planning for booleans.
    /// Plain query → token pruning (`pruned_lex_files`). Boolean query → intersect
    /// the postings of every identifier AND-term (`all_of`, plus `q` if it is an
    /// identifier), rarest list first, so a file missing any required term is
    /// skipped without reading a byte — the selectivity trick that lets an index
    /// beat grep. Non-identifier terms can't be pruned, so we fall back to the
    /// full scoped set (correctness over speed).
    fn planned_lex_files(&self, db: &Db, q: &Query, scoped: &[String]) -> Vec<String> {
        if !has_bool(q) {
            return self.pruned_lex_files(db, q, scoped);
        }
        let mut terms: Vec<String> = q
            .all_of
            .iter()
            .filter(|t| is_plain_ident(t))
            .map(|t| t.to_ascii_lowercase())
            .collect();
        if is_plain_ident(&q.q) {
            terms.push(q.q.to_ascii_lowercase());
        }
        if terms.is_empty() {
            // No prunable AND-terms (only any_of / regex) → scan the scoped set.
            return scoped.to_vec();
        }
        self.and_candidate_files(db, &terms, scoped)
    }

    /// Files containing ALL of `terms` (lowercased identifiers), intersected from
    /// the inverted index rarest-postings-first and restricted to `scoped`.
    fn and_candidate_files(&self, db: &Db, terms: &[String], scoped: &[String]) -> Vec<String> {
        let mut guard = self.token_index.lock().unwrap();
        ensure_token_index(&mut guard, db);
        let postings = &guard.as_ref().unwrap().1.postings;
        // Per term, the files containing SOME identifier that has the term as a
        // substring (SCRY-038, same substring recall as the plain path); any term
        // matching no identifier makes the whole AND empty.
        let mut lists: Vec<HashSet<String>> = Vec::with_capacity(terms.len());
        for t in terms {
            let files = postings_substring(postings, t);
            if files.is_empty() {
                return Vec::new();
            }
            lists.push(files);
        }
        lists.sort_by_key(|v| v.len()); // rarest first → smallest working set
        let scoped_set: HashSet<&str> = scoped.iter().map(|s| s.as_str()).collect();
        let mut acc: HashSet<String> = lists[0]
            .iter()
            .filter(|s| scoped_set.contains(s.as_str()))
            .cloned()
            .collect();
        for l in &lists[1..] {
            acc.retain(|s| l.contains(s));
            if acc.is_empty() {
                break;
            }
        }
        let mut out: Vec<String> = acc.into_iter().collect();
        out.sort();
        out
    }

    fn lexical_ids(
        &self,
        db: &Db,
        q: &Query,
        files: &[String],
        max_hits: usize,
        cands: &mut HashMap<String, Cand>,
    ) -> Vec<String> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for path in files {
            let text = match db.text(path) {
                Some(t) => t,
                None => continue,
            };
            // Boolean refinement (all_of/any_of/none_of) routes through the
            // single-pass Aho-Corasick matcher; the plain case keeps the
            // ripgrep regex path.
            let hits = if has_bool(q) {
                let idx = db.line_index(path);
                lexical::search_bool(
                    &text, &q.q, &q.all_of, &q.any_of, &q.none_of, &idx, max_hits,
                )
            } else {
                lexical::search_text(&text, &q.q, max_hits)
            };
            if hits.is_empty() {
                continue;
            }
            let syms = db.symbols(path);
            for hit in hits {
                let (symbol, start, end) = enclosing(&syms, hit.line);
                let id = make_id(path, symbol.as_deref(), start, end, &text);
                cands.entry(id.clone()).or_insert(Cand {
                    path: path.clone(),
                    symbol,
                    start,
                    end,
                    hit: Some(hit.line),
                });
                *counts.entry(id).or_insert(0) += 1;
            }
        }
        rank_by_count(counts)
    }

    fn structural_ids(
        &self,
        db: &Db,
        q: &Query,
        files: &[String],
        cands: &mut HashMap<String, Cand>,
    ) -> Vec<String> {
        // SCRY-086: smart-case, consistent with lexical search (and the docs): an
        // all-lowercase query matches ANY case; an uppercase letter forces EXACT
        // case (so `Engine` matches `Engine`, not `engine_lower`). Structural used
        // to lowercase both sides unconditionally — case-insensitive even for an
        // explicitly-cased query, contradicting the documented behavior.
        let smart_exact = q.q.bytes().any(|b| b.is_ascii_uppercase());
        let needle = if smart_exact {
            q.q.clone()
        } else {
            q.q.to_ascii_lowercase()
        };
        let mut scored: Vec<(u8, String, String)> = Vec::new(); // (quality, path, id)
        for path in files {
            let text = match db.text(path) {
                Some(t) => t,
                None => continue,
            };
            let syms = db.symbols(path);
            for s in &syms.symbols {
                let name = if smart_exact {
                    s.name.clone()
                } else {
                    s.name.to_ascii_lowercase()
                };
                let quality = if name == needle {
                    0
                } else if name.starts_with(&needle) {
                    1
                } else if name.contains(&needle) {
                    2
                } else {
                    continue;
                };
                let id = make_id(path, Some(&s.name), s.start, s.end, &text);
                cands.entry(id.clone()).or_insert(Cand {
                    path: path.clone(),
                    symbol: Some(s.name.clone()),
                    start: s.start,
                    end: s.end,
                    hit: None,
                });
                scored.push((quality, path.clone(), id));
            }
        }
        scored.sort();
        scored.into_iter().map(|(_, _, id)| id).collect()
    }

    /// NL recall (SCRY-057): a query WORD that EXACTLY names a symbol is a strong
    /// signal the agent referred to it in prose ("how does `pack` choose what to
    /// keep") — the whole-phrase lexical/structural passes miss it. Exact-name
    /// match only, ≥4 chars, minus a few stop-words → low noise. Fused into the
    /// `auto` escalation, so it only runs when the cheap modalities came up weak.
    fn symbol_mention_ids(
        &self,
        db: &Db,
        q: &Query,
        files: &[String],
        cands: &mut HashMap<String, Cand>,
    ) -> Vec<String> {
        const STOP: &[&str] = &[
            "this", "that", "with", "from", "what", "when", "where", "does", "have", "into",
            "then", "they", "will", "your", "some", "more", "than", "using", "used", "make",
            "made", "work", "works", "code", "handle", "return", "value", "call", "called",
            "calls", "file", "files", "text", "name", "names", "path", "paths", "line", "lines",
            "list", "lists", "item", "items", "data", "node", "nodes", "span", "spans", "type",
            "types", "test", "tests", "main", "args", "self", "true", "false", "null", "none",
            "size", "index", "result",
        ];
        let words: HashSet<String> =
            q.q.split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|w| w.len() >= 4 && !STOP.contains(&w.to_ascii_lowercase().as_str()))
                .map(|w| w.to_ascii_lowercase())
                .collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for path in files {
            let text = match db.text(path) {
                Some(t) => t,
                None => continue,
            };
            for s in &db.symbols(path).symbols {
                if words.contains(&s.name.to_ascii_lowercase()) {
                    let id = make_id(path, Some(&s.name), s.start, s.end, &text);
                    cands.entry(id.clone()).or_insert(Cand {
                        path: path.clone(),
                        symbol: Some(s.name.clone()),
                        start: s.start,
                        end: s.end,
                        hit: None,
                    });
                    out.push(id);
                }
            }
        }
        out
    }
    fn expand(&self, db: &Db, q: &Query, id: &str, c: &Cand, score: f64) -> Option<budget::Span> {
        let text = db.text(&c.path)?;
        let lines: Vec<&str> = text.lines().collect();
        // `snippet` may window a LARGE symbol around the actual match (set below),
        // so the line numbering must start at the window's first line, not c.start.
        let mut body_start = c.start;
        let body = match q.detail.as_str() {
            "locate" => {
                let sig = lines.get((c.start as usize).saturating_sub(1)).copied().unwrap_or("");
                format!("L{}-{} {}", c.start, c.end, sig.trim())
            }
            "outline" => {
                let syms = db.symbols(&c.path);
                let self_sym = c.symbol.as_ref().and_then(|name| {
                    syms.symbols.iter().find(|s| &s.name == name && s.start == c.start)
                });
                match self_sym {
                    Some(s) => {
                        let mut out = s.signature.clone();
                        // SCRY-017: for a container, list the nested members'
                        // signatures (bodies elided) so one call shows its shape.
                        if matches!(s.kind, "class" | "struct" | "impl" | "trait" | "interface" | "enum") {
                            let mut members: Vec<&vyer_incr::Symbol> = syms
                                .symbols
                                .iter()
                                .filter(|m| {
                                    m.start > s.start && m.end <= s.end && (m.start, m.end) != (s.start, s.end)
                                })
                                .collect();
                            members.sort_by_key(|m| m.start);
                            for m in members {
                                out.push_str(&format!("\n  {} @L{}-{}", m.signature, m.start, m.end));
                            }
                        }
                        out
                    }
                    None => lines.get((c.start as usize).saturating_sub(1)).copied().unwrap_or("").trim().to_string(),
                }
            }
            // NEW-A/SCRY-120: a `full` search hit returns the SYMBOL's own body
            // (c.start..c.end), so the line numbers added below match the locator's
            // @L range. Slicing the whole file here numbered every line from the
            // symbol's start, mis-targeting any edit keyed off the output.
            "full" => slice_lines(&lines, c.start, c.end, 400),
            _ /* snippet */ => {
                // SCRY-131: a "snippet" must be a SNIPPET. A small symbol shows in
                // full; a large one is WINDOWED around the actual match (the lexical
                // hit line, else the symbol's start) — so one noisy 190-line function
                // can't eat the whole token budget and starve sibling queries in a
                // batch. The locator's @L range still reports the full symbol extent.
                const SNIPPET_FULL: u32 = 40; // ≤ this many lines → show it all
                const SNIPPET_WIN: u32 = 14; // else ± this many lines around the hit
                let span_len = c.end.saturating_sub(c.start);
                if span_len < SNIPPET_FULL {
                    slice_lines(&lines, c.start, c.end, 200)
                } else {
                    let center = c.hit.unwrap_or(c.start).clamp(c.start, c.end);
                    let ws = center.saturating_sub(SNIPPET_WIN).max(c.start);
                    let we = (center + SNIPPET_WIN).min(c.end);
                    body_start = ws;
                    slice_lines(&lines, ws, we, 200)
                }
            }
        };
        // Prefix each line with its real line number so locators are clickable.
        let numbered = number_lines(&body, body_start, q.detail.as_str());
        Some(budget::Span {
            id: id.to_string(),
            text: numbered,
            score,
        })
    }

    // ----- read-by-path (SCRY-003): the `Read` replacement ----------------

    /// Resolve a caller-supplied path to an indexed file. Accepts an exact
    /// repo-relative path, or an unambiguous trailing-segment suffix (so
    /// `game.py` finds `sub/game.py` when there's exactly one).
    fn resolve_indexed_path(&self, db: &Db, path: &str) -> Option<String> {
        let files = db.files();
        if files.iter().any(|f| f == path) {
            return Some(path.to_string());
        }
        let matches: Vec<&String> = files
            .iter()
            .filter(|f| {
                f.ends_with(path)
                    && (f.len() == path.len() || f.as_bytes()[f.len() - path.len() - 1] == b'/')
            })
            .collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
        None
    }

    /// Return the whole file at `path` as a single span. `detail`:
    /// `full` → the file line-numbered (capped to the token budget),
    /// `outline` → its symbol signatures, `locate` → a one-line summary.
    fn read_path_spans(
        &self,
        db: &Db,
        q: &Query,
        path: &str,
        budget_tokens: usize,
    ) -> Vec<budget::Span> {
        let resolved = match self.resolve_indexed_path(db, path) {
            Some(p) => p,
            None => {
                // SCRY-083: a failed read is usually a TYPO or a wrong directory,
                // not an absolute path. Suggest the closest indexed file(s) by
                // basename edit-distance (≤2 catches a one/two-char typo AND a
                // right-name/wrong-dir) so the agent recovers without a `tree` call.
                let files = db.files();
                let want = path.rsplit('/').next().unwrap_or(path);
                let mut sugg: Vec<(usize, &String)> = files
                    .iter()
                    .filter_map(|f| {
                        let d = levenshtein(f.rsplit('/').next().unwrap_or(f), want);
                        (d <= 2).then_some((d, f))
                    })
                    .collect();
                sugg.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
                let hint = if sugg.is_empty() {
                    "check the spelling, or use detail=tree to list files".to_string()
                } else {
                    let names: Vec<&str> = sugg.iter().take(3).map(|(_, f)| f.as_str()).collect();
                    format!(
                        "did you mean: {} (or detail=tree to list files)",
                        names.join(", ")
                    )
                };
                return vec![budget::Span {
                    id: format!("{path}#read"),
                    text: format!("file not indexed: {path} — {hint}"),
                    score: 1.0,
                }];
            }
        };
        let text = match db.text(&resolved) {
            Some(t) => t,
            None => return Vec::new(),
        };

        // Line-range read (head / tail / sed -n): resolve against the warm-core
        // line-offset index and slice only the requested bytes — no full-file
        // line scan, no `Vec<&str>` of every line.
        if let Some(spec) = &q.lines {
            return self.line_range_span(db, &resolved, &text, spec, budget_tokens);
        }

        let lines: Vec<&str> = text.lines().collect();
        let n = lines.len() as u32;
        let body = match q.detail.as_str() {
            "locate" => {
                let syms = db.symbols(&resolved);
                format!("{} lines, {} symbols", lines.len(), syms.symbols.len())
            }
            "outline" => {
                let out = db.outline(&resolved);
                if out.lines.is_empty() {
                    format!("{} lines, no extractable symbols", lines.len())
                } else {
                    // SCRY-063: a huge file's full signature list is ONE span; if it
                    // exceeds the budget it gets packed out entirely (the agent sees
                    // nothing). Cap it to fit — as many signatures as the budget
                    // allows, then note the remainder — so the outline is always useful.
                    let mut acc = String::new();
                    let mut used = 0usize;
                    let mut shown = 0usize;
                    for line in &out.lines {
                        let cost = budget::est_tokens(line) + 1;
                        if shown > 0 && used + cost > budget_tokens.saturating_sub(40) {
                            break;
                        }
                        if shown > 0 {
                            acc.push('\n');
                        }
                        acc.push_str(line);
                        used += cost;
                        shown += 1;
                    }
                    if shown < out.lines.len() {
                        acc.push_str(&format!(
                            "\n… +{} more symbols (narrow with a smaller path_scope, or detail=locate)",
                            out.lines.len() - shown
                        ));
                    }
                    acc
                }
            }
            _ => {
                // Full read, line-numbered. Cap to the token budget (leaving room
                // for the envelope) so a large file degrades to a head + note
                // instead of being dropped whole by the packer.
                let char_budget = budget_tokens.saturating_sub(64).saturating_mul(4);
                let mut s = String::new();
                let mut shown = 0usize;
                for (i, l) in lines.iter().enumerate() {
                    let line = format!("{}: {}\n", i + 1, l);
                    if s.len() + line.len() > char_budget {
                        break;
                    }
                    s.push_str(&line);
                    shown += 1;
                }
                if shown < lines.len() {
                    s.push_str(&format!(
                        "… {} more lines omitted (budget); raise budget_tokens or query a symbol with detail=snippet\n",
                        lines.len() - shown
                    ));
                }
                s
            }
        };
        let id = make_id(&resolved, None, 1, n.max(1), &text);
        vec![budget::Span {
            id,
            text: body,
            score: 1.0,
        }]
    }

    /// Read a 1-based inclusive line range of `resolved`, the head/tail/sed -n
    /// replacement. Resolves the range against the memoized line-offset index and
    /// copies only the requested bytes (the warm-core advantage: O(range), not
    /// an O(n) newline scan, and no `Vec<&str>` of every line). Numbering starts
    /// at the real first line so locators stay clickable. Budget-capped: an
    /// over-budget range degrades to a head + a note, never a dropped span.
    fn line_range_span(
        &self,
        db: &Db,
        resolved: &str,
        text: &str,
        spec: &str,
        budget_tokens: usize,
    ) -> Vec<budget::Span> {
        let idx = db.line_index(resolved);
        let total = idx.len() as u32;
        let (start, end) = match parse_line_range(spec, total) {
            Ok(r) => r,
            Err(hint) => {
                return vec![budget::Span {
                    id: format!("{resolved}#lines"),
                    text: format!("bad line range {spec:?}: {hint}"),
                    score: 1.0,
                }];
            }
        };
        // Byte offsets: start of line `start`, up to the start of the line after
        // `end` (or EOF for the final line). Slicing here can never split a UTF-8
        // char — every offset is a line start or EOF (see `Db::line_index`).
        let start_off = idx[(start - 1) as usize] as usize;
        let end_off = idx
            .get(end as usize)
            .map(|o| *o as usize)
            .unwrap_or(text.len());
        let slice = &text[start_off..end_off];

        let char_budget = budget_tokens.saturating_sub(64).saturating_mul(4);
        let mut body = String::new();
        let mut shown = 0u32;
        for (i, line) in slice.lines().enumerate() {
            let numbered = format!("{}: {}\n", start + i as u32, line);
            if !body.is_empty() && body.len() + numbered.len() > char_budget {
                break;
            }
            body.push_str(&numbered);
            shown += 1;
        }
        let requested = end - start + 1;
        if shown < requested {
            body.push_str(&format!(
                "… {} more lines in range omitted (budget); raise budget_tokens or narrow `lines`\n",
                requested - shown
            ));
        }
        let id = make_id(resolved, None, start, end, text);
        vec![budget::Span {
            id,
            text: body,
            score: 1.0,
        }]
    }

    /// SCRY-116: `mode=diagnose` — map a compiler/test/stack-trace blob (`q`) to the exact
    /// code it references: the enclosing symbol's locator + a short window with the failing
    /// line marked `>>`. The first reference scores highest (root cause → attention edge).
    fn diagnose_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let refs = parse_diagnostics(&q.q);
        if refs.is_empty() {
            return vec![budget::Span {
                id: "diagnose".into(),
                text: "no `file:line` locations found — pass the compiler/test output or a stack \
                       trace as `q` (mode=diagnose)\n"
                    .into(),
                score: 1.0,
            }];
        }
        let n = refs.len() as f64;
        let mut spans = Vec::new();
        for (i, d) in refs.into_iter().enumerate() {
            let (path, line) = (d.path.clone(), d.line);
            let score = 1.0 - (i as f64) / (n + 1.0);
            let resolved = match self.resolve_indexed_path(db, &path) {
                Some(r) => r,
                None => {
                    spans.push(budget::Span {
                        id: format!("{path}@L{line}"),
                        text: format!(
                            "{path}:{line} — not in vyer's index (outside the repo, gitignored, \
                             generated, or a dependency); read it with native tools\n"
                        ),
                        score,
                    });
                    continue;
                }
            };
            let text = match db.text(&resolved) {
                Some(t) => t,
                None => continue,
            };
            let idx = db.line_index(&resolved);
            let total = idx.len().max(1) as u32;
            let line = line.clamp(1, total);
            let syms = db.symbols(&resolved);
            let (sym, s_start, s_end) = enclosing(&syms, line);
            // SCRY-133: a STRUCTURED header — file:line, severity, enclosing symbol,
            // and the message — so an agent reads the failure as data, not prose.
            let mut body = String::new();
            body.push_str(&format!(
                "{resolved}:{line} {} in {}",
                d.severity.as_deref().unwrap_or("diag"),
                sym.as_deref().unwrap_or("(top-level)")
            ));
            if let Some(m) = &d.message {
                body.push_str(&format!(" :: {m}"));
            }
            body.push('\n');
            let lo = line.saturating_sub(2).max(1);
            let hi = (line + 2).min(total);
            for ln in lo..=hi {
                let off = idx[(ln - 1) as usize] as usize;
                let end = idx
                    .get(ln as usize)
                    .map(|o| *o as usize)
                    .unwrap_or(text.len());
                let content = text[off..end].trim_end_matches('\n');
                body.push_str(&format!(
                    "{} {ln}: {content}\n",
                    if ln == line { ">>" } else { "  " }
                ));
            }
            let id = match &sym {
                Some(name) => make_id(&resolved, Some(name), s_start, s_end, &text),
                None => make_id(&resolved, None, line, line, &text),
            };
            spans.push(budget::Span {
                id,
                text: body,
                score,
            });
        }
        spans
    }

    /// SCRY-118: `detail=import` — resolve `q` (a symbol) to its defining file via the index, and
    /// (with `path` = the file to import INTO) build the exact import statement for that file's
    /// language. Vyer knows where every symbol lives; this hands the agent the precise line to add.
    fn import_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let sym = q.q.trim();
        if sym.is_empty() {
            return vec![budget::Span {
                id: "import".into(),
                text: "detail=import needs q=<SymbolName>; add path=<file to import INTO> for the exact statement\n".into(),
                score: 1.0,
            }];
        }
        let mut defs: Vec<String> = Vec::new();
        for f in db.files() {
            if db.symbols(&f).symbols.iter().any(|s| s.name == sym) {
                defs.push(f);
            }
        }
        if defs.is_empty() {
            return vec![budget::Span {
                id: format!("import:{sym}"),
                text: format!(
                    "`{sym}` is not defined in any indexed file — check the name, or it lives in a \
                     dependency (dependency-source navigation is on the roadmap)\n"
                ),
                score: 1.0,
            }];
        }
        let target = q
            .path
            .as_deref()
            .and_then(|p| self.resolve_indexed_path(db, p));
        let mut out = String::new();
        for def in &defs {
            if Some(def.as_str()) == target.as_deref() {
                out.push_str(&format!(
                    "`{sym}` is already in this file ({def}) — no import needed\n"
                ));
            } else if let Some(t) = &target {
                out.push_str(&format!("{}\n", build_import(sym, t, def)));
            } else {
                out.push_str(&format!(
                    "`{sym}` is defined in {def}; pass path=<file to import INTO> for the exact import line\n"
                ));
            }
        }
        if defs.len() > 1 {
            out.push_str(&format!(
                "(note: {sym} is defined in {} files — pick the intended one)\n",
                defs.len()
            ));
        }
        vec![budget::Span {
            id: format!("import:{sym}"),
            text: out,
            score: 1.0,
        }]
    }

    /// `detail=count` — the grep -c / wc -l replacement, as one summary span.
    ///
    /// With `path`: report the file's line / byte / symbol counts (wc -l, plus
    /// structure grep can't give). Otherwise count `q` across the scoped repo:
    /// for a plain identifier we prune to candidate files via the inverted index
    /// and count with a single SIMD `memmem` pass (case-sensitive, like grep -c);
    /// for a regex we smart-case-match. We report BOTH the number of matching
    /// lines (grep -c) and the total match count (grep -o | wc -l), which grep
    /// makes you run two commands to get.
    fn count_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        // Single-file count (wc -l + structure).
        if let Some(p) = &q.path {
            let span = match self.resolve_indexed_path(db, p) {
                Some(resolved) => {
                    let lines = db.line_index(&resolved).len();
                    let bytes = db.text(&resolved).map(|t| t.len()).unwrap_or(0);
                    let syms = db.symbols(&resolved).symbols.len();
                    budget::Span {
                        id: format!("{resolved}#count"),
                        text: format!("{resolved}: {lines} lines, {bytes} bytes, {syms} symbols"),
                        score: 1.0,
                    }
                }
                None => budget::Span {
                    id: format!("{p}#count"),
                    text: format!("file not indexed: {p}"),
                    score: 1.0,
                },
            };
            return vec![span];
        }

        // Boolean count (SCRY-044): count matching LINES per file for an
        // all_of/any_of/none_of predicate — the boolean search you can already run,
        // now countable. Candidate files are pruned via the AND postings
        // (`planned_lex_files`); each file's lines are tested in one Aho-Corasick
        // pass (`search_bool`).
        if has_bool(q) {
            let scoped = self.scoped_files(db, q);
            let cand = self.planned_lex_files(db, q, &scoped);
            let mut per: Vec<(String, usize)> = Vec::new();
            let mut tot_lines = 0usize;
            for path in &cand {
                let text = match db.text(path) {
                    Some(t) => t,
                    None => continue,
                };
                let idx = db.line_index(path);
                let hits = lexical::search_bool(
                    &text,
                    &q.q,
                    &q.all_of,
                    &q.any_of,
                    &q.none_of,
                    &idx,
                    usize::MAX,
                );
                if !hits.is_empty() {
                    per.push((path.clone(), hits.len()));
                    tot_lines += hits.len();
                }
            }
            per.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut label: Vec<String> = Vec::new();
            if !q.all_of.is_empty() {
                label.push(format!("all[{}]", q.all_of.join(",")));
            }
            if !q.any_of.is_empty() {
                label.push(format!("any[{}]", q.any_of.join(",")));
            }
            if !q.none_of.is_empty() {
                label.push(format!("not[{}]", q.none_of.join(",")));
            }
            if !q.q.trim().is_empty() {
                label.push(format!("q={}", q.q.trim()));
            }
            let label = label.join(" ");
            let mut body = format!(
                "{label}: {tot_lines} matching lines across {} files\n",
                per.len()
            );
            const CAP: usize = 200;
            for (i, (path, lines)) in per.iter().enumerate() {
                if i == CAP {
                    body.push_str(&format!("… {} more files omitted\n", per.len() - CAP));
                    break;
                }
                body.push_str(&format!(
                    "{}: {lines} lines\n",
                    output::sanitize_field(path)
                ));
            }
            return vec![budget::Span {
                id: "count:bool".into(),
                text: body,
                score: 1.0,
            }];
        }

        let needle = q.q.trim();
        if needle.is_empty() {
            return vec![budget::Span {
                id: "count#error".into(),
                text: "count needs a query `q`, a boolean (all_of/any_of/none_of), or a `path` for line counts".into(),
                score: 1.0,
            }];
        }

        let scoped = self.scoped_files(db, q);
        let literal = is_plain_ident(needle);
        // Literal queries prune to files known to contain the token; regex can't.
        let cand = if literal {
            self.pruned_lex_files(db, q, &scoped)
        } else {
            scoped
        };

        let mut per: Vec<(String, usize, usize)> = Vec::new();
        let (mut tot_lines, mut tot_matches) = (0usize, 0usize);
        for path in &cand {
            let text = match db.text(path) {
                Some(t) => t,
                None => continue,
            };
            let idx = db.line_index(path);
            let (lines, matches) = if literal {
                lexical::count_literal(&text, needle, &idx)
            } else {
                lexical::count_regex(&text, needle, &idx)
            };
            if matches > 0 {
                per.push((path.clone(), lines, matches));
                tot_lines += lines;
                tot_matches += matches;
            }
        }
        // Most-hit files first; path asc for a deterministic tie-break.
        per.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut body = format!(
            "\"{needle}\": {tot_matches} matches on {tot_lines} lines across {} files\n",
            per.len()
        );
        let cap = 200usize; // keep the summary compact; the total is already exact
        for (i, (path, lines, matches)) in per.iter().enumerate() {
            if i == cap {
                body.push_str(&format!("… {} more files omitted\n", per.len() - cap));
                break;
            }
            body.push_str(&format!(
                "{}: {lines} lines ({matches} matches)\n",
                output::sanitize_field(path)
            ));
        }
        vec![budget::Span {
            id: format!("count:{needle}"),
            text: body,
            score: 1.0,
        }]
    }

    /// `detail=tree` — the ls / find / tree replacement, as one span. Walks the
    /// resident, ignore-filtered, lexicographically-sorted file set (`db.files()`)
    /// — no `readdir`/`stat` syscall storm, no re-applying ignore rules. Filters:
    /// `path` (directory prefix), `path_scope` globs + `lang` (via `scoped_files`),
    /// and `q` (path substring). Renders an indented tree in a single linear pass
    /// over the sorted paths (the same trick `tree(1)` uses).
    fn tree_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let prefix = q.path.as_deref().map(|p| p.trim_end_matches('/'));
        let needle = q.q.trim();
        let mut paths: Vec<String> = self
            .scoped_files(db, q)
            .into_iter()
            .filter(|p| match prefix {
                Some(dir) if !dir.is_empty() => p == dir || p.starts_with(&format!("{dir}/")),
                _ => true,
            })
            .filter(|p| needle.is_empty() || p.contains(needle))
            .collect();
        paths.sort(); // scoped_files already sorts, but be explicit for the render

        if paths.is_empty() {
            return vec![budget::Span {
                id: "tree:empty".into(),
                text: "0 files match (check path / path_scope / lang / q)".into(),
                score: 1.0,
            }];
        }

        let total = paths.len();
        // Compact directory-grouped tree. Cap lines to stay budget-friendly.
        let cap = 400usize;
        let mut out = format!("{total} files\n");
        let mut prev: Vec<&str> = Vec::new();
        for (shown, p) in paths.iter().enumerate() {
            if shown >= cap {
                out.push_str(&format!(
                    "… {} more files omitted (narrow with path/q)\n",
                    total - shown
                ));
                break;
            }
            let comps: Vec<&str> = p.split('/').collect();
            let mut shared = 0;
            while shared < prev.len() && shared < comps.len() && prev[shared] == comps[shared] {
                shared += 1;
            }
            for (d, comp) in comps.iter().enumerate().skip(shared) {
                let indent = "  ".repeat(d);
                if d + 1 == comps.len() {
                    out.push_str(&format!("{indent}{comp}\n"));
                } else {
                    out.push_str(&format!("{indent}{comp}/\n"));
                }
            }
            prev = comps;
        }
        let label = prefix.filter(|d| !d.is_empty()).unwrap_or(".");
        vec![budget::Span {
            id: format!("tree:{label}"),
            text: out,
            score: 1.0,
        }]
    }

    // ----- the graph `refs` path (Phase 5, approximate) -------------------

    /// Resolve definition(s) of `q.q` and the cross-file sites that reference it.
    /// Without an LSP this is a tree-sitter (defs) + word-boundary lexical (refs)
    /// approximation, so every result is tagged `graph=partial(approx)` and the
    /// agent can calibrate. Emits one span per definition plus a references span.
    /// SCRY-134 (guiding master): code references to `name` OUTSIDE its own
    /// definition in `def_file` — the blast radius for the safe-delete guard.
    /// Word-boundary, code-identifier-only (comments/strings excluded), pruned by a
    /// cheap resident-text substring check. graph=partial(approx): a same-named
    /// symbol elsewhere counts conservatively (better to refuse a delete than
    /// silently break callers); `force:true` overrides.
    fn external_refs(&self, db: &Db, name: &str, def_file: &str) -> Vec<(String, u32)> {
        let pattern = format!(r"\b{}\b", regex_escape_ident(name));
        let def_lines: Vec<u32> = db
            .symbols(def_file)
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.start)
            .collect();
        let mut out: Vec<(String, u32)> = Vec::new();
        for f in db.files() {
            let text = match db.text(&f) {
                Some(t) => t,
                None => continue,
            };
            if !text.contains(name) {
                continue; // cheap prune over resident text
            }
            let flang = vyer_incr::detect_lang(&f);
            let code_lines = code_ident_lines(&text, name, flang);
            for hit in lexical::search_text(&text, &pattern, 200) {
                let is_own_def = f == def_file && def_lines.contains(&hit.line);
                if is_own_def || !code_lines.contains(&hit.line) {
                    continue;
                }
                out.push((f.clone(), hit.line));
            }
        }
        out
    }

    /// SCRY-137: symbols whose name is within a small edit distance of `q.q` — the
    /// "did you mean" recovery for a mistyped identifier. Scoped + length-pruned +
    /// bounded Levenshtein (early-exit), so it's cheap even though it only runs on
    /// the no-match path. Threshold scales with name length (3-7→1, 8-11→2, 12+→3).
    fn fuzzy_symbol_matches(&self, db: &Db, q: &Query) -> Vec<(String, vyer_incr::Symbol, u32)> {
        let needle = q.q.trim().to_ascii_lowercase();
        let nlen = needle.chars().count();
        if nlen < 3 {
            return Vec::new();
        }
        let max_dist = (nlen / 4).clamp(1, 3) as u32;
        let mut out: Vec<(String, vyer_incr::Symbol, u32)> = Vec::new();
        for f in self.scoped_files(db, q) {
            for s in &db.symbols(&f).symbols {
                let slen = s.name.chars().count();
                if slen.abs_diff(nlen) > max_dist as usize {
                    continue; // length prune before the (bounded) distance compute
                }
                if let Some(d) = levenshtein_within(&needle, &s.name.to_ascii_lowercase(), max_dist)
                {
                    if d > 0 {
                        out.push((f.clone(), s.clone(), d));
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.2.cmp(&b.2)
                .then(a.1.name.len().cmp(&b.1.name.len()))
                .then(a.0.cmp(&b.0))
        });
        out.dedup_by(|a, b| a.0 == b.0 && a.1.name == b.1.name && a.1.start == b.1.start);
        out
    }

    fn refs_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        // SCRY-119: accept a fully-qualified `PATH#SYMBOL` locator (the playbook's
        // disambiguation form), not just a bare name — otherwise the whole locator
        // is taken as the symbol name and resolves to nothing (false "0 refs").
        let (qpath, name) = parse_symbol_query(&q.q);
        let name = name.as_str();
        let mut files = self.scoped_files(db, q);
        let resolved_def = qpath
            .as_deref()
            .and_then(|p| self.resolve_indexed_path(db, p));
        // SCRY-126: a qualified `PATH#SYMBOL` scopes refs to the definition's
        // PACKAGE (nearest ancestor with a manifest), so a same-named symbol in
        // another monorepo package isn't conflated — the precision a bare name
        // can't give. graph stays lexical-approx WITHIN that package.
        let mut scope_note = String::new();
        if let Some(r) = &resolved_def {
            if let Some(pkg) = package_root(&files, r).filter(|p| !p.is_empty()) {
                let pref = format!("{pkg}/");
                files.retain(|f| f.starts_with(&pref));
                scope_note = format!(" scope=package({pkg})");
            }
        }
        // The definition is pinned to the qualified file when given; references are
        // gathered across the (now package-scoped) file set by name below.
        let def_files: Vec<String> = match &resolved_def {
            Some(r) => vec![r.clone()],
            None => files.clone(),
        };

        // definitions: exact-name symbols (tree-sitter spans when available).
        let mut defs: Vec<(String, vyer_incr::Symbol)> = Vec::new();
        for f in &def_files {
            for s in &db.symbols(f).symbols {
                if s.name == name {
                    defs.push((f.clone(), s.clone()));
                }
            }
        }

        // references: word-boundary matches of the name, excluding definition
        // header lines (those are the defs, not references to them).
        let pattern = format!(r"\b{}\b", regex_escape_ident(name));
        let mut refs: Vec<(String, u32, String)> = Vec::new();
        // SCRY-061: prune the reference scan to files that actually contain the
        // name (inverted index), like lexical search — refs no longer search_text +
        // code_ident_lines every scoped file. The def loop above used only cheap
        // memoized symbol tables, so it stays over the full set.
        // SCRY-119: prune by the PARSED name, not the raw `q.q` (which may be a
        // `PATH#SYMBOL` locator the token index can't match — that returned 0 refs).
        let nq = Query {
            q: name.to_string(),
            ..q.clone()
        };
        let ref_files = self.pruned_lex_files(db, &nq, &files);
        for f in &ref_files {
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            // SCRY-059: only lines where the name is a CODE identifier count — a
            // mention in a comment or string is not a reference (same precision as
            // context/impact got in SCRY-043).
            let flang = vyer_incr::detect_lang(f);
            let code_lines = code_ident_lines(&text, name, flang);
            for hit in lexical::search_text(&text, &pattern, 200) {
                let is_def_line = defs.iter().any(|(df, s)| df == f && s.start == hit.line);
                if is_def_line || !code_lines.contains(&hit.line) {
                    continue;
                }
                refs.push((f.clone(), hit.line, hit.text.trim().to_string()));
            }
        }

        let mut spans = Vec::new();
        for (f, s) in &defs {
            let text = db.text(f).unwrap_or_else(|| std::sync::Arc::from(""));
            let id = make_id(f, Some(name), s.start, s.end, &text);
            spans.push(budget::Span {
                id,
                text: format!("def [{}] {}  graph=partial(approx)", s.kind, s.signature),
                score: 1.0,
            });
        }

        let cap = q.k.max(1) * 6;
        let shown = refs.len().min(cap);
        let mut body = format!(
            "references to `{name}`: defs={} refs={} (showing {}) graph=partial(approx) tier=lexical-approx{scope_note}\n",
            defs.len(),
            refs.len(),
            shown
        );
        for (f, line, t) in refs.iter().take(cap) {
            body.push_str(&format!("{f}:{line}: {t}\n"));
        }
        spans.push(budget::Span {
            id: format!("{name}#refs"),
            text: body,
            score: 0.5,
        });
        spans
    }

    // ----- the impact / blast-radius path (SP-3, approximate) -------------

    /// Transitive referrers of `q.q`: the symbols that reference it, the symbols
    /// that reference *those*, and so on (a word-boundary approximation over the
    /// symbol bodies, honestly tagged `graph=partial`). One call answers "what
    /// breaks if I change this?". Depth-capped and dedup'd.
    fn impact_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        // SCRY-119: accept a fully-qualified `PATH#SYMBOL` locator, not just a bare
        // name — otherwise impact treats the whole locator as the name, finds no
        // referrers, and dangerously reports "safe to change in isolation".
        let (qpath, target) = parse_symbol_query(&q.q);
        let mut files = self.scoped_files(db, q);
        // SCRY-126: a qualified locator scopes the blast radius to the definition's
        // package, so impact doesn't conflate a same-named symbol elsewhere.
        let mut scope_note = String::new();
        if let Some(r) = qpath
            .as_deref()
            .and_then(|p| self.resolve_indexed_path(db, p))
        {
            if let Some(pkg) = package_root(&files, &r).filter(|p| !p.is_empty()) {
                let pref = format!("{pkg}/");
                files.retain(|f| f.starts_with(&pref));
                scope_note = format!(" scope=package({pkg})");
            }
        }

        // One pass: collect every symbol and build an inverted index
        // `referrers[name] = symbol indices whose body mentions `name``. BFS then
        // becomes O(results) lookups instead of re-scanning every body per depth
        // (which was O(depth·files·symbols·scan) — seconds on a 2k-file repo).
        let mut symbols: Vec<(String, vyer_incr::Symbol)> = Vec::new();
        let mut referrers: HashMap<String, Vec<usize>> = HashMap::new();
        for f in &files {
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            let lines: Vec<&str> = text.lines().collect();
            for s in &db.symbols(f).symbols {
                let bs = (s.start as usize).saturating_sub(1);
                let be = (s.end as usize).min(lines.len());
                if bs >= be {
                    continue;
                }
                let idx = symbols.len();
                // Comment/string-aware (SCRY-043): a name mentioned only in a
                // comment or string is NOT a real referrer, so it never enters the
                // blast radius. Keeps `detail=impact` honest about what breaks.
                let iflang = vyer_incr::detect_lang(f);
                let mut refs = scan_idents(&lines[bs..be].join("\n"), false, iflang);
                refs.remove(&s.name); // don't count self-reference
                for name in refs {
                    referrers.entry(name).or_default().push(idx);
                }
                symbols.push((f.clone(), s.clone()));
            }
        }

        let mut impacted: Vec<(usize, u32)> = Vec::new(); // symbol index, depth
        let mut seen_idx: HashSet<usize> = HashSet::new();
        let mut seen_name: HashSet<String> = HashSet::new();
        seen_name.insert(target.clone());
        let mut frontier = vec![target.clone()];
        for depth in 1..=5u32 {
            if frontier.is_empty() {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for name in frontier.drain(..) {
                if let Some(idxs) = referrers.get(&name) {
                    for &i in idxs {
                        if seen_idx.insert(i) {
                            impacted.push((i, depth));
                            if seen_name.insert(symbols[i].1.name.clone()) {
                                next.push(symbols[i].1.name.clone());
                            }
                        }
                    }
                }
            }
            frontier = next;
        }

        impacted.sort_by(|a, b| a.1.cmp(&b.1).then(symbols[a.0].0.cmp(&symbols[b.0].0)));
        let total = impacted.len();
        // Direct (depth-1) referrers are the ones that break *immediately* if the
        // symbol changes — the actionable number; deeper levels are the (safely
        // over-reported, lexical-approx) ripple. Surface both.
        let direct = impacted.iter().filter(|(_, d)| *d == 1).count();
        let mut body = format!(
            "impact of `{target}`: {direct} direct + {} transitive referrer(s) ({total} total) graph=partial(approx) tier=lexical-approx{scope_note}\n",
            total.saturating_sub(direct)
        );
        if total == 0 {
            body.push_str("  (no referrers — safe to change in isolation)\n");
        } else {
            const CAP: usize = 60;
            for (i, depth) in impacted.iter().take(CAP) {
                let (f, s) = &symbols[*i];
                body.push_str(&format!("  d{depth} {f}#{} @L{}\n", s.name, s.start));
            }
            if total > CAP {
                body.push_str(&format!("  … and {} more\n", total - CAP));
            }
        }
        vec![budget::Span {
            id: format!("{target}#impact"),
            text: body,
            score: 1.0,
        }]
    }

    /// One-call context pack for a symbol (SP-9): its definition (full snippet)
    /// plus the symbols it calls (callees), the symbols that call it (callers),
    /// and its tests — everything an agent needs to understand it, assembled and
    /// edge-ordered in a single response instead of 4–8 separate calls.
    fn context_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        // SCRY-119: accept a fully-qualified `PATH#SYMBOL` locator, not just a bare
        // name (else the whole locator is taken as the symbol name -> "no symbol
        // named …"). A resolved path qualifier pins the definition to that file.
        let (qpath, target) = parse_symbol_query(&q.q);
        let def_file = qpath
            .as_deref()
            .and_then(|p| self.resolve_indexed_path(db, p));
        let files = self.scoped_files(db, q);
        // Index symbols by name and capture each symbol's referenced identifiers.
        let mut by_name: HashSet<String> = HashSet::new();
        let mut all: Vec<(String, vyer_incr::Symbol, HashSet<String>)> = Vec::new();
        let mut defs: Vec<(String, vyer_incr::Symbol)> = Vec::new();
        // Cheap pass: every symbol NAME (callee resolution needs the full set) and
        // the target's definition(s) — memoized symbol tables only, no body scan.
        for f in &files {
            for s in &db.symbols(f).symbols {
                by_name.insert(s.name.clone());
                if s.name == target && def_file.as_ref().is_none_or(|df| df == f) {
                    defs.push((f.clone(), s.clone()));
                }
            }
        }
        // SCRY-062: the expensive `scan_idents`-per-body pass (caller detection)
        // runs ONLY over files that contain the target name — a CALLER must
        // reference the target, so a file without it can hold no caller (same
        // sound prune as refs/SCRY-061). On large repos this is the difference
        // between O(all symbols) and O(symbols in name-containing files).
        // SCRY-119: prune by the PARSED target name, not the raw `q.q` (a
        // `PATH#SYMBOL` locator wouldn't match the token index).
        let nq = Query {
            q: target.clone(),
            ..q.clone()
        };
        let caller_files = self.pruned_lex_files(db, &nq, &files);
        for f in &caller_files {
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            let lines: Vec<&str> = text.lines().collect();
            let flang = vyer_incr::detect_lang(f);
            for s in &db.symbols(f).symbols {
                let bs = (s.start as usize).saturating_sub(1);
                let be = (s.end as usize).min(lines.len());
                let toks = if bs < be {
                    // Comment/string-aware (SCRY-043): a name mentioned only in a
                    // comment or string isn't a real reference, so it won't make
                    // this symbol a false caller of the target.
                    scan_idents(&lines[bs..be].join("\n"), false, flang)
                } else {
                    HashSet::new()
                };
                all.push((f.clone(), s.clone(), toks));
            }
        }
        if defs.is_empty() {
            return vec![budget::Span {
                id: format!("{target}#context"),
                text: format!(
                    "no symbol named `{target}` found (try detail=locate or mode=semantic)"
                ),
                score: 1.0,
            }];
        }

        // Callees: identifiers actually CALLED in the def body (name followed by
        // `(`, comments/strings skipped) that name known symbols (SCRY-043). The
        // call-site requirement keeps params and comment words out of `[calls]`.
        let def = &defs[0];
        let callees: Vec<String> = {
            let dtext = db.text(&def.0).unwrap_or_else(|| std::sync::Arc::from(""));
            let dlines: Vec<&str> = dtext.lines().collect();
            let bs = (def.1.start as usize).saturating_sub(1);
            let be = (def.1.end as usize).min(dlines.len());
            let body = if bs < be {
                dlines[bs..be].join("\n")
            } else {
                String::new()
            };
            // A RECURSIVE symbol calls itself — a real callee. Its declaration is
            // one `target(` call-site, so it recurses iff `target(` appears ≥2×;
            // then keep `target`, else exclude it (it's just the declaration).
            let recursive = count_call_sites(&body, &target) >= 2;
            let dflang = vyer_incr::detect_lang(&def.0);
            let mut c: Vec<String> = scan_idents(&body, true, dflang)
                .into_iter()
                .filter(|t| (recursive || *t != target) && by_name.contains(t))
                .collect();
            c.sort();
            c
        };

        // Callers: symbols whose body references the target (excluding defs).
        let mut callers: Vec<(String, String, u32)> = all
            .iter()
            .filter(|(_, s, toks)| s.name != target && toks.contains(&target))
            .map(|(f, s, _)| (f.clone(), s.name.clone(), s.start))
            .collect();
        callers.sort();
        let tests: Vec<&(String, String, u32)> = callers
            .iter()
            .filter(|(_, n, _)| n.starts_with("test"))
            .collect();

        // Assemble: a summary span + the definition's full snippet (edge-ordered).
        let mut summary = format!(
            "context for `{target}`: {} def(s), {} callee(s), {} caller(s), {} test(s)\n",
            defs.len(),
            callees.len(),
            callers.len(),
            tests.len()
        );
        summary.push_str(&format!(
            "[calls] {}\n",
            if callees.is_empty() {
                "—".into()
            } else {
                callees.join(", ")
            }
        ));
        summary.push_str("[called by]\n");
        for (f, n, s) in callers.iter().take(40) {
            let tag = if n.starts_with("test") { " (test)" } else { "" };
            summary.push_str(&format!("  {f}#{n} @L{s}{tag}\n"));
        }
        if callers.is_empty() {
            summary.push_str("  — (no callers found)\n");
        }
        // SCRY-053: with multiple defs of this name, the [calls] + snippet are for
        // the FIRST only — say so and list the others so the agent can target one.
        if defs.len() > 1 {
            summary.push_str(&format!(
                "note: {} symbols named `{target}`; [calls]/snippet are for {}#{}@L{}. Disambiguate with a specific locator:\n",
                defs.len(),
                def.0,
                def.1.name,
                def.1.start
            ));
            for (f, s) in defs.iter().skip(1).take(8) {
                summary.push_str(&format!("  {f}#{}@L{}-{}\n", s.name, s.start, s.end));
            }
        }
        let mut spans = vec![budget::Span {
            id: format!("{target}#context"),
            text: summary,
            score: 2.0,
        }];
        // The definition body itself (highest value → edge position).
        let text = db.text(&def.0).unwrap_or_else(|| std::sync::Arc::from(""));
        let id = make_id(&def.0, Some(&def.1.name), def.1.start, def.1.end, &text);
        let cand = Cand {
            path: def.0.clone(),
            symbol: Some(def.1.name.clone()),
            start: def.1.start,
            end: def.1.end,
            hit: None,
        };
        let snippet_q = Query {
            detail: "snippet".into(),
            ..q.clone()
        };
        if let Some(span) = self.expand(db, &snippet_q, &id, &cand, 3.0) {
            spans.push(span);
        }
        spans
    }

    // ----- AST-pattern structural search (SP-7) ---------------------------

    /// Run the tree-sitter query in `q.q` over scoped files and return matched
    /// node spans. Files whose grammar rejects the query (e.g. a Python query on
    /// a Rust file) are skipped; the compile error is surfaced only if nothing
    /// matched anywhere. Scope with `lang=` / `path_scope` to target one grammar.
    fn ast_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        // A `path` scopes the structural query to a single file (mirrors
        // detail=ast); otherwise it runs over the scoped set (path_scope/lang).
        let files = if let Some(p) = &q.path {
            match self.resolve_indexed_path(db, p) {
                Some(r) => vec![r],
                None => {
                    return vec![budget::Span {
                        id: format!("{p}#ast"),
                        text: format!("file not indexed: {p}"),
                        score: 1.0,
                    }]
                }
            }
        } else {
            self.scoped_files(db, q)
        };
        let mut spans = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut err: Option<String> = None;
        for f in &files {
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            match vyer_index::ast_query(&text, db.lang(f), &q.q) {
                Ok(matches) => {
                    let lines: Vec<&str> = text.lines().collect();
                    for m in matches {
                        let key = format!("{f}:{}:{}", m.start, m.end);
                        if !seen.insert(key) {
                            continue;
                        }
                        let sig = lines
                            .get((m.start as usize).saturating_sub(1))
                            .copied()
                            .unwrap_or("")
                            .trim();
                        spans.push(budget::Span {
                            id: make_id(f, None, m.start, m.end, &text),
                            text: format!("L{}-{} [{}] {}", m.start, m.end, m.kind, sig),
                            score: 1.0,
                        });
                    }
                }
                Err(e) => err = Some(e),
            }
        }
        if spans.is_empty() {
            let why = err.unwrap_or_else(|| "0 matches".into());
            return vec![budget::Span {
                id: "ast/result".into(),
                text: format!(
                    "AST query — {why}. pattern: {} (tips: scope with lang= so the query compiles against one grammar; run detail=ast on a file to see its node kinds and author the pattern)",
                    q.q
                ),
                score: 1.0,
            }];
        }
        spans.truncate(q.k.max(1) * 4);
        spans
    }

    /// SP-13: dump a file's tree-sitter AST node-kinds (the `dump_ast`
    /// affordance) so an agent can author `mode=ast` queries. Needs `path`.
    fn dump_ast_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let path = match &q.path {
            Some(p) => p,
            None => {
                return vec![budget::Span {
                    id: "ast#dump".into(),
                    text: "detail=ast needs a `path` (the file whose AST to dump — use it to author mode=ast queries)".into(),
                    score: 1.0,
                }]
            }
        };
        let resolved = match self.resolve_indexed_path(db, path) {
            Some(r) => r,
            None => {
                return vec![budget::Span {
                    id: format!("{path}#ast"),
                    text: format!("file not indexed: {path}"),
                    score: 1.0,
                }]
            }
        };
        let text = match db.text(&resolved) {
            Some(t) => t,
            None => return Vec::new(),
        };
        // Scope the dump: a `q` symbol name dumps just that symbol's AST (the
        // ergonomic path — no line math); else an optional `lines` filter scopes
        // to a region; else the whole file. Line numbers are preserved either way.
        let range = if !q.q.trim().is_empty() {
            let name = q.q.trim();
            let syms = db.symbols(&resolved);
            match syms.symbols.iter().find(|s| s.name == name) {
                Some(s) => Some((s.start, s.end)),
                None => {
                    let names: Vec<&str> = syms
                        .symbols
                        .iter()
                        .map(|s| s.name.as_str())
                        .take(20)
                        .collect();
                    return vec![budget::Span {
                        id: format!("{resolved}#ast"),
                        text: format!(
                            "symbol `{name}` not found in {resolved}; symbols: {}",
                            names.join(", ")
                        ),
                        score: 1.0,
                    }];
                }
            }
        } else {
            q.lines.as_deref().and_then(|spec| {
                let total = text.lines().count() as u32;
                parse_line_range(spec, total).ok()
            })
        };
        match vyer_index::dump_ast(&text, db.lang(&resolved), 400, range) {
            Ok(tree) => vec![budget::Span {
                id: format!("{resolved}#ast"),
                text: if tree.trim().is_empty() {
                    "no named AST nodes (empty file or an unsupported language)".into()
                } else {
                    tree
                },
                score: 1.0,
            }],
            Err(e) => vec![budget::Span {
                id: format!("{resolved}#ast"),
                text: format!("AST dump failed: {e}"),
                score: 1.0,
            }],
        }
    }

    /// SP-10: a cumulative *session* diff, built from the SP-6 history stack.
    /// For every file an agent touched via `code_apply` this session we recover
    /// its session-original text (the earliest snapshot across all batches) and
    /// diff it against the current warm-core text. `q` (path substring) and
    /// `path_scope` (globs) filter which files show. Deterministic, in-process,
    /// no `git` — the "what have I changed so far?" an agent otherwise has to
    /// reconstruct by hand or shell out for.
    fn diff_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        use std::collections::BTreeMap;
        // Earliest recorded pre-text per path = its state at session start.
        // `None` = the file did not exist then (created this session).
        let mut original: BTreeMap<String, Option<String>> = BTreeMap::new();
        {
            let history = self.history.lock().unwrap();
            for batch in history.iter() {
                for (path, pre) in batch {
                    original.entry(path.clone()).or_insert_with(|| pre.clone());
                }
            }
        }

        let mut spans = Vec::new();
        for (path, orig) in &original {
            if !q.q.is_empty() && !path.contains(&q.q) {
                continue;
            }
            if !path_in_scope(path, &q.path_scope) {
                continue;
            }
            let new: String = db.text(path).map(|t| t.to_string()).unwrap_or_default();
            // SCRY-123 (#7): a file CREATED this session has no session-start text,
            // so a unified diff renders its WHOLE content as `+` lines — turning
            // detail=diff into a full-repo dump. Summarize instead; the content is
            // readable via `path`. Real deltas are still shown for MODIFIED files.
            if orig.is_none() {
                let added = new.lines().count();
                spans.push(budget::Span {
                    id: format!("{path}#diff"),
                    text: format!(
                        "{path}: created NEW file (+{added} lines) — read it with code {{ path:\"{path}\" }}"
                    ),
                    score: 1.0,
                });
                continue;
            }
            let old = orig.clone().unwrap_or_default();
            if old == new {
                continue; // net-zero: edited then reverted/undone this session
            }
            // A modified file: the real delta, capped so one huge change can't
            // dominate the budget (SCRY-123).
            let body = cap_diff(&apply::line_diff(path, &old, &new), 160);
            spans.push(budget::Span {
                id: format!("{path}#diff"),
                text: format_diff(&body, path),
                score: 1.0,
            });
        }

        if spans.is_empty() {
            let msg = if original.is_empty() {
                "no edits applied this session (detail=diff is built from code_apply history)"
                    .to_string()
            } else {
                format!(
                    "{} file(s) touched this session, but none match the filter or all are net-zero",
                    original.len()
                )
            };
            spans.push(budget::Span {
                id: "diff#none".to_string(),
                text: msg,
                score: 1.0,
            });
        }
        spans
    }

    /// SP-11: a subtree symbol outline. Returns one span per in-scope file with
    /// its symbol signatures (bodies elided), letting an agent orient on a
    /// directory in a single call instead of N per-file reads. Scope comes from
    /// `scoped_files` (path_scope incl. `!` exclusions, and `lang`).
    fn outline_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let mut files = self.scoped_files(db, q);
        files.sort();
        let mut spans = Vec::new();
        for f in &files {
            let out = db.outline(f);
            if out.lines.is_empty() {
                continue;
            }
            spans.push(budget::Span {
                id: f.clone(),
                text: out.lines.join("\n"),
                score: 1.0,
            });
        }
        if spans.is_empty() {
            spans.push(budget::Span {
                id: "outline#none".into(),
                text: "no symbols in scope (widen path_scope, or drop the lang filter)".into(),
                score: 1.0,
            });
        }
        spans
    }

    /// SCRY-039: the fusable form of the semantic modality. Scores symbols by
    /// subword-TF-IDF overlap with `q` (identical scoring to `semantic_spans`) but
    /// returns ranked candidate *ids* (populating `cands`) instead of rendered
    /// spans, so `mode=auto` can RRF-fuse it with lexical/structural on a
    /// low-confidence escalation (Rule §5). Deterministic, zero-dependency.
    fn semantic_ids(
        &self,
        db: &Db,
        q: &Query,
        files: &[String],
        cands: &mut HashMap<String, Cand>,
    ) -> Vec<String> {
        let qtoks = subword_tokens(&q.q);
        if qtoks.is_empty() {
            return Vec::new();
        }
        // Build-or-reuse the whole-repo semantic index (revision-keyed cache).
        let mut guard = self.semantic_index.lock().unwrap();
        let rev = db.revision();
        if guard.as_ref().map(|(r, _)| *r != rev).unwrap_or(true) {
            *guard = Some((rev, build_semantic_index(db)));
        }
        let idx = &guard.as_ref().unwrap().1;
        // Per-query scope filter (path_scope/lang) over the cached corpus.
        let scoped: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
        let n = idx.docs.len().max(1) as f64;
        let mut scored: Vec<(f64, usize)> = Vec::new();
        // SCRY-080: only docs that share a query subword can score > 0 (the score
        // increments solely when `d.2.contains(qt)`), so score the candidate union
        // from the inverted index instead of the whole corpus — provably the same
        // result set, O(candidates) not O(corpus).
        let mut candidates: HashSet<usize> = HashSet::new();
        for qt in &qtoks {
            if let Some(docs) = idx.postings.get(qt) {
                candidates.extend(docs.iter().copied());
            }
        }
        for &i in &candidates {
            let d = &idx.docs[i];
            if !scoped.contains(d.0.as_str()) {
                continue;
            }
            let mut score = 0.0;
            for qt in &qtoks {
                if d.2.contains(qt) {
                    let idf = (n / (*idx.df.get(qt).unwrap_or(&1) as f64)).ln() + 1.0;
                    score += idf;
                }
            }
            if score > 0.0 {
                scored.push((score, i));
            }
        }
        // Deterministic order (Rule §9): score desc, then doc index asc as a stable
        // tie-break — independent of the unordered `candidates` iteration.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut ids = Vec::new();
        for (_score, i) in scored.into_iter().take(q.k.max(1)) {
            let (f, sym, _) = &idx.docs[i];
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            let id = make_id(f, Some(&sym.name), sym.start, sym.end, &text);
            cands.entry(id.clone()).or_insert(Cand {
                path: f.clone(),
                symbol: Some(sym.name.clone()),
                start: sym.start,
                end: sym.end,
                hit: None,
            });
            ids.push(id);
        }
        ids
    }
    /// Conceptual retrieval (SP-7 semantic) — deterministic subword TF-IDF over
    /// symbol names + signatures. Splits camelCase/snake_case so a natural-language
    /// query ("validate auth token") ranks `validate_token` highly even when the
    /// agent doesn't know the exact identifier. Honest: lexical-subword, not neural.
    fn semantic_spans(&self, db: &Db, q: &Query) -> Vec<budget::Span> {
        let qtoks = subword_tokens(&q.q);
        if qtoks.is_empty() {
            return vec![budget::Span {
                id: "semantic/result".into(),
                text: "semantic: query had no usable terms".into(),
                score: 1.0,
            }];
        }
        // SCRY-080: reuse the revision-cached whole-repo index (was rebuilt INLINE
        // every query here — ~95ms on a 50k-file repo) and score only the candidate
        // docs that share a query subword (the inverted index), then a per-query
        // `scoped` filter. Collect the top-k under the lock; expand after releasing.
        let files = self.scoped_files(db, q);
        let scoped: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();
        let picks: Vec<(String, vyer_incr::Symbol, f64)> = {
            let mut guard = self.semantic_index.lock().unwrap();
            let rev = db.revision();
            if guard.as_ref().map(|(r, _)| *r != rev).unwrap_or(true) {
                *guard = Some((rev, build_semantic_index(db)));
            }
            let idx = &guard.as_ref().unwrap().1;
            let n = idx.docs.len().max(1) as f64;
            let mut candidates: HashSet<usize> = HashSet::new();
            for qt in &qtoks {
                if let Some(d) = idx.postings.get(qt) {
                    candidates.extend(d.iter().copied());
                }
            }
            let mut scored: Vec<(f64, usize)> = Vec::new();
            for &i in &candidates {
                let d = &idx.docs[i];
                if !scoped.contains(d.0.as_str()) {
                    continue;
                }
                let mut score = 0.0;
                for qt in &qtoks {
                    if d.2.contains(qt) {
                        score += (n / (*idx.df.get(qt).unwrap_or(&1) as f64)).ln() + 1.0;
                    }
                }
                if score > 0.0 {
                    scored.push((score, i));
                }
            }
            let max = scored
                .iter()
                .map(|(s, _)| *s)
                .fold(0.0_f64, f64::max)
                .max(1e-9);
            // Deterministic (Rule §9): score desc, doc index asc tie-break.
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.1.cmp(&b.1))
            });
            scored
                .into_iter()
                .take(q.k.max(1))
                .map(|(score, i)| {
                    let (f, sym, _) = &idx.docs[i];
                    (f.clone(), sym.clone(), score / max)
                })
                .collect()
        };
        if picks.is_empty() {
            return vec![budget::Span {
                id: "semantic/result".into(),
                text: format!(
                    "semantic(lexical-subword): no symbol shares a term with `{}`; try mode=ast or a keyword",
                    q.q
                ),
                score: 1.0,
            }];
        }
        let mut spans = Vec::new();
        for (f, sym, nscore) in picks {
            let text = match db.text(&f) {
                Some(t) => t,
                None => continue,
            };
            let id = make_id(&f, Some(&sym.name), sym.start, sym.end, &text);
            let cand = Cand {
                path: f.clone(),
                symbol: Some(sym.name.clone()),
                start: sym.start,
                end: sym.end,
                hit: None,
            };
            if let Some(span) = self.expand(db, q, &id, &cand, nscore) {
                spans.push(span);
            }
        }
        spans
    }

    // ----- the `code_apply` tool ------------------------------------------

    /// SCRY-071: push a batch onto the undo history, bounded to MAX_UNDO_BATCHES
    /// so a long-lived daemon's memory stays flat across a heavy editing session
    /// (each batch retains pre-edit file text). The oldest batches drop — undo
    /// reaches the most recent MAX_UNDO_BATCHES, which is far beyond real use.
    fn record_history(&self, batch: EditBatch) {
        let mut hist = self.history.lock().unwrap();
        hist.push(batch);
        if hist.len() > MAX_UNDO_BATCHES {
            let excess = hist.len() - MAX_UNDO_BATCHES;
            hist.drain(0..excess);
        }
    }

    /// SCRY-069/070: is `target` writable WITHOUT changing anything on disk? An
    /// existing file must not be read-only; a NEW file's nearest existing ancestor
    /// dir must be writable. Pre-flighting this lets a multi-file write (or an
    /// undo) refuse up front instead of partially applying — which would lose data
    /// (a half-done move) or undo history (a half-done revert).
    fn is_writable_target(&self, target: &std::path::Path) -> bool {
        if target.exists() {
            std::fs::metadata(target)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false)
        } else {
            let mut anc = target.parent();
            while let Some(p) = anc {
                if p.exists() {
                    break;
                }
                anc = p.parent();
            }
            anc.and_then(|p| std::fs::metadata(p).ok())
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false)
        }
    }

    /// SCRY-069: atomically commit the buffered disk ops. PRE-FLIGHT — verify every
    /// write target is writable BEFORE changing any file, so a partial flush (e.g.
    /// a `move`'s source cut, then the dest write fails) can't lose data; refuse
    /// otherwise ("no files changed"). On any residual error the warm core is
    /// reconciled with ACTUAL disk content (SCRY-068) so no query returns an
    /// un-persisted edit. Multi-edit writes to one file are deduped (last wins).
    fn commit_pending(
        &self,
        db: &mut Db,
        pending: &[DiskOp],
        originals: &std::collections::HashMap<String, Option<String>>,
    ) -> Result<(), String> {
        let reconcile = |db: &mut Db| {
            for rel in originals.keys() {
                match std::fs::read_to_string(self.config.root.join(rel)) {
                    Ok(actual) => {
                        db.set_text(rel, &actual);
                    }
                    Err(_) => {
                        db.remove_text(rel);
                    }
                }
            }
        };
        // Dedupe writes by target (a multi-edit batch re-writes the whole file per
        // edit; the LAST has the final text); collect deletes separately.
        let mut writes: std::collections::HashMap<&std::path::PathBuf, &String> =
            std::collections::HashMap::new();
        let mut deletes: Vec<&std::path::PathBuf> = Vec::new();
        for op in pending {
            match op {
                DiskOp::Write(p, t) => {
                    writes.insert(p, t);
                }
                DiskOp::Delete(p) => deletes.push(p),
            }
        }
        // Pre-flight: EVERY write target must be writable BEFORE we change any file
        // — so a partial flush (e.g. a `move`'s source cut, then the dest write
        // fails) can't lose data. Direct write respects the file's read-only bit
        // (a temp+rename would bypass it).
        for &target in writes.keys() {
            if !self.is_writable_target(target) {
                reconcile(db);
                return Err(format!(
                    "commit refused: `{}` is not writable (no files changed)",
                    target.display()
                ));
            }
        }
        // Commit (now expected to succeed). A residual failure (e.g. disk full)
        // still reconciles the warm core with disk (SCRY-068) so no query sees an
        // un-persisted edit.
        for (target, text) in &writes {
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // SCRY-102: preserve the file's existing EOL convention — if the on-disk
            // file is CRLF-dominant, normalize the spliced text to CRLF so the edit
            // doesn't introduce mixed LF/CRLF (a noisy whole-line diff on Windows
            // projects). New / LF files keep LF; untouched CRLF lines are unchanged.
            let out: std::borrow::Cow<str> = match std::fs::read_to_string(target) {
                Ok(orig) if is_crlf_dominant(&orig) => std::borrow::Cow::Owned(to_crlf(text)),
                _ => std::borrow::Cow::Borrowed(text.as_str()),
            };
            if let Err(err) = std::fs::write(target, out.as_bytes()) {
                reconcile(db);
                return Err(format!("commit failed: {err}"));
            }
        }
        for p in &deletes {
            if let Err(err) = std::fs::remove_file(p) {
                reconcile(db);
                return Err(format!("commit failed: {err}"));
            }
        }
        Ok(())
    }
    /// SCRY-031: run the operator-configured verify command (argv) in the repo
    /// root after a successful write, and return a one-line inline report so the
    /// agent learns whether the edit *compiles*, not just *parses*. The command
    /// is fixed at launch (never request-driven), so this is not a generic shell
    /// surface. Best-effort: a missing/failed-to-spawn command degrades to a note.
    /// Run an OPERATOR-configured argv in the repo root and return a STRUCTURED
    /// report (ok, or parsed diagnostics). Shared by the post-write verify
    /// (verify_cmd) and the request-triggered `code_run` task — BOTH are
    /// operator-allowlisted argv, never a request-supplied command string, so
    /// Rule §3 (typed ops only, no shell passthrough) holds. `kind` labels the
    /// report ("verify" / "run"). Best-effort: a spawn failure degrades to a note.
    fn run_argv(&self, argv: &[String], kind: &str) -> String {
        let label = argv.join(" ");
        let Some((prog, args)) = argv.split_first() else {
            return format!("{kind}: empty command (operator misconfiguration)\n");
        };
        match std::process::Command::new(prog)
            .args(args)
            .current_dir(&self.config.root)
            .output()
        {
            Ok(o) if o.status.success() => {
                self.record(kind, format!("{label} ok"));
                format!("{kind}({label})=ok\n")
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let out = String::from_utf8_lossy(&o.stdout);
                self.record(kind, format!("{label} FAILED"));
                // SCRY-135: pipe the build/test output through the STRUCTURED
                // diagnostics parser (the back-half of mode=diagnose) so the agent
                // reads "what I broke" as data — file:line [severity] :: message —
                // not a single hand-picked error line.
                let combined = format!("{err}\n{out}");
                let diags = parse_diagnostics(&combined);
                let mut report = String::new();
                if diags.is_empty() {
                    // No file:line refs parsed — fall back to the first error-ish line.
                    let pick = err
                        .lines()
                        .chain(out.lines())
                        .find(|l| {
                            let t = l.to_ascii_lowercase();
                            t.contains("error") || t.contains("failed")
                        })
                        .or_else(|| {
                            err.lines()
                                .chain(out.lines())
                                .find(|l| !l.trim().is_empty())
                        })
                        .unwrap_or("(no output)")
                        .trim();
                    report.push_str(&format!("{kind}({label})=FAILED: {pick}\n"));
                } else {
                    let shown = diags.len().min(10);
                    report.push_str(&format!(
                        "{kind}({label})=FAILED: {} diagnostic(s) (showing {shown})\n",
                        diags.len()
                    ));
                    for d in diags.iter().take(10) {
                        report.push_str(&format!(
                            "  {}:{} {} :: {}\n",
                            d.path,
                            d.line,
                            d.severity.as_deref().unwrap_or("diag"),
                            d.message.as_deref().unwrap_or("(see output)")
                        ));
                    }
                    report.push_str(
                        "  → paste the build output into `code` mode=diagnose for enclosing symbols + code windows\n",
                    );
                }
                report
            }
            Err(e) => format!("{kind}({label})=ERROR: could not run `{label}` ({e})\n"),
        }
    }

    fn run_verify(&self) -> Option<String> {
        let argv = self.config.verify_cmd.as_ref()?;
        let mut report = self.run_argv(argv, "verify");
        // SCRY-055: the write already committed (verify runs post-apply and does not
        // roll back, so multi-step refactors aren't blocked). Tell the agent so it
        // can `undo:1` if this edit caused the failure.
        if report.contains("=FAILED") {
            report.push_str(
                "  note: the edit IS written; if it caused this, code_apply undo:1 to revert\n",
            );
        }
        Some(report)
    }

    /// SCRY-140 `code_run`: execute an OPERATOR-allowlisted task by NAME and return
    /// structured diagnostics — closing the agent's edit→build/test→fix loop inside
    /// the tool. The request selects a task name only; it can NEVER supply a command
    /// or args (Rule §3). Gated by `--allow-run` (a distinct effect class from
    /// writes). Reached via `code_apply {"run":"test"}`, so the tool count stays at
    /// two (Rule §1).
    pub fn code_run(&self, task: &str) -> Result<String, String> {
        if !self.config.allow_run {
            self.record("code_run", "DENIED: run disabled".into());
            return Err("command execution is disabled for this session — start the server with \
                 `--allow-run` and register tasks via `--run name=\"<cmd>\"` (operator-allowlisted; \
                 the request selects a task NAME, never a command)"
                .into());
        }
        let task = task.trim();
        match self.config.run_tasks.get(task) {
            Some(argv) => {
                self.record("code_run", format!("task={task}"));
                Ok(self.run_argv(argv, "run"))
            }
            None => {
                let avail: Vec<&str> = self.config.run_tasks.keys().map(String::as_str).collect();
                let list = if avail.is_empty() {
                    "(none — operator registered no --run tasks)".to_string()
                } else {
                    avail.join(", ")
                };
                Err(format!(
                    "unknown run task `{task}`. Configured tasks: {list}. Tasks are operator-allowlisted; \
                     select one by NAME (you cannot pass a command or args)."
                ))
            }
        }
    }

    pub fn code_apply(&self, req: &ApplyRequest) -> Result<String, String> {
        // SCRY-140 code_run: a `run` request executes (an allowlisted task) but does
        // NOT write — so it's gated by --allow-run, not --allow-writes, and handled
        // before the write gate. Selecting a task by name only (Rule §3).
        if let Some(task) = &req.run {
            return self.code_run(task);
        }
        if !self.config.allow_writes {
            self.record("code_apply", "DENIED: writes disabled".into());
            return Err("writes are disabled for this session (start with --allow-writes)".into());
        }
        let mut db = self.db.lock().unwrap();

        // ---- SP-6 UNDO: revert the last N batches ----
        if let Some(n) = req.undo {
            let mut hist = self.history.lock().unwrap();
            let mut report = String::new();
            let mut batches = 0usize;
            let mut files = 0usize;
            for _ in 0..n {
                if hist.is_empty() {
                    break;
                }
                // SCRY-070: pre-flight the batch (PEEK, don't pop) — if any restore
                // target is read-only, refuse with the history INTACT rather than
                // popping it, failing the write, and losing the undo record.
                {
                    let batch = hist.last().unwrap();
                    for (path, orig) in batch.iter() {
                        if orig.is_some() && !self.is_writable_target(&self.config.root.join(path))
                        {
                            return Err(format!(
                                "undo refused: `{path}` is not writable (history intact, no files changed)"
                            ));
                        }
                    }
                }
                let batch = hist.pop().unwrap();
                for (path, orig) in batch {
                    let target = sandbox::validate_write(&self.config.root, &path)
                        .map_err(|r| format!("undo: write denied ({r:?}) for {path}"))?;
                    match orig {
                        Some(text) => {
                            if let Some(parent) = target.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            std::fs::write(&target, &text)
                                .map_err(|err| format!("undo write failed for {path}: {err}"))?;
                            db.set_text(&path, &text);
                        }
                        None => {
                            let _ = std::fs::remove_file(&target);
                            db.remove_text(&path);
                        }
                    }
                    files += 1;
                }
                batches += 1;
            }
            if batches == 0 {
                return Err("undo: nothing to undo (history empty)".into());
            }
            report.push_str(&format!(
                "undo: reverted {batches} batch(es), restored {files} file(s); warm core updated\n"
            ));
            self.record("code_apply", format!("UNDO {batches} batch(es)"));
            return Ok(report);
        }

        // SP-2 atomic multi-edit: update the warm core per edit (intra-batch
        // freshness) but DEFER disk writes and snapshot each touched file. If any
        // edit fails, roll the warm core back and touch no files (all-or-nothing);
        // only on full success do we flush the buffered writes to disk.
        let mut originals: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let mut pending: Vec<DiskOp> = Vec::new();

        // SCRY-128: tool-authored guards (returned as isError so the model reacts),
        // not raw serde/empty-report surprises.
        if req.edits.is_empty() {
            return Err(
                "no edits given. Send {\"locator\":\"src/x.rs#foo\",\"new_body\":\"…\"} for one edit, \
                 {\"edits\":[…]} for a batch, or {\"undo\":N} to roll back. \
                 Tip: add \"dry_run\":true to preview the unified diff without writing (no Read needed; bytes never enter your context)"
                    .into(),
            );
        }
        if let Some(e) = req.edits.iter().find(|e| e.locator.trim().is_empty()) {
            let op = if e.anchor.is_some() {
                "anchor/replace"
            } else if e.new_body.is_some() {
                "new_body"
            } else {
                "this"
            };
            return Err(format!(
                "missing `locator` for the {op} edit. Every edit needs a locator naming WHERE to edit: \
                 `PATH#SYMBOL` (a symbol), `PATH` alone (file/module-level, e.g. for anchor/replace on imports), \
                 or an authoring directive like `PATH#@new` / `PATH#@after:SYMBOL`"
            ));
        }

        // SCRY-134 (guiding master): refuse to DELETE a symbol that still has
        // references — the dead-code / break-callers mistake an agent makes when it
        // reasons "this looks unused" without the graph. The warm ref graph KNOWS;
        // surface the sites and refuse, overridable with force:true. Pre-flight
        // (before any mutation) so the batch stays all-or-nothing.
        for e in &req.edits {
            if e.force {
                continue;
            }
            let raw = e.locator.split(" :: ").next().unwrap_or(&e.locator).trim();
            let Some((p, t)) = raw.split_once('#') else {
                continue;
            };
            let Some(sym) = t.strip_prefix("@delete:") else {
                continue;
            };
            let Some(def_file) = self.resolve_indexed_path(&db, p) else {
                continue;
            };
            let refs = self.external_refs(&db, sym, &def_file);
            if !refs.is_empty() {
                let shown: Vec<String> = refs
                    .iter()
                    .take(8)
                    .map(|(f, l)| format!("{f}:{l}"))
                    .collect();
                let more = refs.len().saturating_sub(shown.len());
                let tail = if more > 0 {
                    format!(", +{more} more")
                } else {
                    String::new()
                };
                return Err(format!(
                    "refusing to delete `{sym}`: {} reference(s) still point to it (graph=partial/approx) — \
                     update or remove the callers first, or pass force:true to delete anyway. sites: {}{tail}",
                    refs.len(),
                    shown.join(", ")
                ));
            }
        }

        // SCRY-139 (guiding master): refuse a rename onto a name that ALREADY exists
        // as a symbol — a symbol-aware rename would then merge/shadow two distinct
        // symbols (a subtle correctness mistake). Name the existing site(s); force:true
        // overrides. Pre-flight (no mutation yet).
        for e in &req.edits {
            if e.force {
                continue;
            }
            let Some(new_name) = e.rename.as_deref().map(str::trim).filter(|s| !s.is_empty())
            else {
                continue;
            };
            let raw = e.locator.split(" :: ").next().unwrap_or(&e.locator).trim();
            let old = raw
                .split_once('#')
                .map(|(_, t)| t.split('@').next().unwrap_or(t))
                .map(|t| t.rsplit(['.', ':']).next().unwrap_or(t))
                .unwrap_or("");
            if new_name == old {
                continue;
            }
            let scope: Vec<String> = if e.path_scope.is_empty() {
                db.files()
            } else {
                db.files()
                    .into_iter()
                    .filter(|f| path_in_scope(f, &e.path_scope))
                    .collect()
            };
            let mut collisions: Vec<String> = Vec::new();
            for f in &scope {
                for s in &db.symbols(f).symbols {
                    if s.name == new_name {
                        collisions.push(format!("{f}:{}", s.start));
                    }
                }
            }
            if !collisions.is_empty() {
                let shown: Vec<String> = collisions.iter().take(5).cloned().collect();
                let more = collisions.len().saturating_sub(shown.len());
                let tail = if more > 0 {
                    format!(", +{more} more")
                } else {
                    String::new()
                };
                return Err(format!(
                    "refusing to rename `{old}`→`{new_name}`: a symbol named `{new_name}` already exists \
                     ({} site(s): {}{tail}) — the rename would collide/merge them. Pick a free name, scope \
                     with path_scope, or pass force:true.",
                    collisions.len(),
                    shown.join(", ")
                ));
            }
        }

        let result: Result<String, String> = (|| {
            let mut report = String::new();
            for e in &req.edits {
                // Parse the locator into a path and an optional target (a symbol,
                // `symbol@Lrange`, or an `@directive`). Path-only is valid for an
                // anchored file-scope edit.
                let raw = e.locator.split(" :: ").next().unwrap_or(&e.locator).trim();
                let (path, target) = match raw.split_once('#') {
                    Some((p, t)) if !p.is_empty() && !t.is_empty() => {
                        (p.to_string(), Some(t.to_string()))
                    }
                    Some(_) => return Err(format!("malformed locator: {}", e.locator)),
                    None => (raw.to_string(), None),
                };

                // SCRY-056: honor the locator's staleness hash (CLAUDE.md §5). If
                // the locator carries `:: blake3=HEX` and names a plain symbol, the
                // symbol's content must still hash to HEX — otherwise it changed
                // since the agent read it and the edit would be based on stale text.
                // (Symbol-anchored, so line drift alone never trips this; only a
                // content change does.) We reject ONLY when the symbol exists but no
                // same-named symbol matches the hash — a missing symbol falls
                // through to the normal "not found" path.
                if let Some(want) = e
                    .locator
                    .split(" :: ")
                    .nth(1)
                    .and_then(|s| s.trim().strip_prefix("blake3="))
                {
                    if let Some(t) = &target {
                        if !t.starts_with('@') {
                            let sym_name = t.split("@L").next().unwrap_or(t);
                            if let Some(text) = db.text(&path) {
                                let syms = db.symbols(&path);
                                let mut exists = false;
                                let mut any_match = false;
                                for s in syms.symbols.iter().filter(|s| s.name == sym_name) {
                                    exists = true;
                                    if short_hash(&symbol_slice(&text, s.start, s.end)) == want {
                                        any_match = true;
                                        break;
                                    }
                                }
                                if exists && !any_match {
                                    return Err(format!(
                                        "stale locator: `{sym_name}` in {path} changed since you read it (blake3 {want} no longer matches); re-query for a fresh locator, then retry"
                                    ));
                                }
                            }
                        }
                    }
                }

                // ---- SUPERPOWER: symbol-aware repo-wide rename (SCRY-027) ----
                // Renames the symbol's definition AND every whole-word reference
                // across the repo. Each touched file is re-parsed; if ANY would break,
                // nothing is written (all-or-nothing). The safe cross-file refactor a
                // coding agent can't get from `sed`/`Grep`+`Edit`.
                if let Some(newname) = &e.rename {
                    let old = target
                        .as_deref()
                        .filter(|t| !t.starts_with('@') && !t.contains("@L"))
                        .ok_or_else(|| {
                            format!("rename needs locator `PATH#SYMBOL`: {}", e.locator)
                        })?;
                    if !is_ident(newname) {
                        return Err(format!(
                            "rename target `{newname}` is not a valid identifier"
                        ));
                    }
                    // tree-sitter's error-recovery won't flag a keyword used as an
                    // identifier, so guard it explicitly (closes the lenient-gate hole
                    // for the rename path — SP-8).
                    if is_language_keyword(newname) {
                        return Err(format!(
                        "rename target `{newname}` is a reserved keyword (would compile-break); choose another name"
                    ));
                    }
                    // Phase 1: build + validate every changed file (no writes yet).
                    let mut changes: Vec<(String, String, std::path::PathBuf, usize)> = Vec::new();
                    let mut total = 0usize;
                    // SCRY-119: a repo-wide rename is whole-word + comment/string aware,
                    // but it is NOT cross-language symbol resolution — a same-named symbol
                    // in another language (or prose in a `.md`) is a DIFFERENT thing.
                    // Confine to the defining symbol's own language family so renaming a
                    // Python class can't rewrite an unrelated TypeScript interface or
                    // README text. `path_scope` gives finer control.
                    let def_lang = match db.lang(&path) {
                        vyer_incr::Lang::Generic => vyer_incr::detect_lang(&path),
                        l => l,
                    };
                    // SCRY-126: if `old` is DEFINED in more than one file of this
                    // language (a duplicated name across monorepo packages) and no
                    // path_scope was given, a repo-wide rename would corrupt the OTHER
                    // package's same-named symbol. Confine to the definition's package
                    // (nearest ancestor with a manifest). path_scope overrides.
                    let auto_pkg: Option<String> = if e.path_scope.is_empty() {
                        let defs = db
                            .files()
                            .into_iter()
                            .filter(|f| same_lang_family(db.lang(f), def_lang))
                            .filter(|f| db.symbols(f).symbols.iter().any(|s| s.name == old))
                            .count();
                        if defs > 1 {
                            package_root(&db.files(), &path).filter(|p| !p.is_empty())
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    for f in db.files() {
                        if !e.path_scope.is_empty() && !path_in_scope(&f, &e.path_scope) {
                            continue;
                        }
                        // SCRY-119: skip files outside the defining symbol's language
                        // family (a same-named symbol elsewhere is a different symbol).
                        if !same_lang_family(db.lang(&f), def_lang) {
                            continue;
                        }
                        // SCRY-126: ambiguous name — confine to the def's package.
                        if let Some(pkg) = &auto_pkg {
                            if !f.starts_with(&format!("{pkg}/")) {
                                continue;
                            }
                        }
                        let text = match db.text(&f) {
                            Some(t) => t,
                            None => continue,
                        };
                        let flang = db.lang(&f);
                        let (newtext, n) = replace_word(&text, old, newname, flang);
                        if n == 0 {
                            continue;
                        }
                        if vyer_index::has_parse_error(&newtext, flang) {
                            return Err(format!(
                            "rename aborted: `{old}`→`{newname}` would break {f} (does not parse); no files changed"
                        ));
                        }
                        let tgt = sandbox::validate_write(&self.config.root, &f)
                            .map_err(|r| format!("rename aborted: write denied ({r:?}) for {f}"))?;
                        total += n;
                        changes.push((f, newtext, tgt, n));
                    }
                    if changes.is_empty() {
                        return Err(format!(
                            "rename: symbol `{old}` not found in any indexed file"
                        ));
                    }
                    report.push_str(&format!(
                        "rename `{old}` → `{newname}`: {total} occurrence(s) across {} file(s)\n",
                        changes.len()
                    ));
                    if let Some(pkg) = &auto_pkg {
                        report.push_str(&format!(
                            "note: `{old}` is defined in multiple packages; confined to `{pkg}/` (pass path_scope to widen or pick another package)\n"
                        ));
                    }
                    for (f, _, _, n) in &changes {
                        report.push_str(&format!("  {f}: {n}\n"));
                    }
                    // Phase 2: commit (only after every file validated).
                    if req.dry_run {
                        report.push_str("(dry_run: not written)\n");
                    } else {
                        for (f, newtext, tgt, _) in &changes {
                            snapshot(&mut originals, &db, f);
                            db.set_text(f, newtext);
                            pending.push(DiskOp::Write(tgt.clone(), newtext.clone()));
                        }
                        report.push_str("(renamed; parse=ok all files; warm core updated)\n");
                        self.record(
                            "code_apply",
                            format!("RENAME {old} -> {newname} ({} files)", changes.len()),
                        );
                    }
                    report.push('\n');
                    continue;
                }

                // ---- SUPERPOWER: bulk anchored search-replace across a glob (SP-4) ----
                // A glob locator (`src/**`, `**/*.py`) with an anchor replaces EVERY
                // occurrence in every matching file — atomic + parse-validated. `sed`
                // across a repo with a safety net.
                if let (Some(anchor), true) = (&e.anchor, path.contains('*')) {
                    let replace = e.replace.as_deref().unwrap_or("");
                    if anchor.is_empty() {
                        return Err("bulk replace needs a non-empty anchor".into());
                    }
                    let mut changes: Vec<(String, String, std::path::PathBuf, usize)> = Vec::new();
                    let mut total = 0usize;
                    for f in db.files() {
                        if !glob_match(&path, &f) {
                            continue;
                        }
                        let text = match db.text(&f) {
                            Some(t) => t,
                            None => continue,
                        };
                        let (newtext, n) = replace_all_str(&text, anchor, replace);
                        if n == 0 {
                            continue;
                        }
                        if vyer_index::has_parse_error(&newtext, db.lang(&f)) {
                            return Err(format!(
                            "bulk replace aborted: editing {f} would not parse; no files changed"
                        ));
                        }
                        let tgt = sandbox::validate_write(&self.config.root, &f)
                            .map_err(|r| format!("bulk replace: write denied ({r:?}) for {f}"))?;
                        total += n;
                        changes.push((f, newtext, tgt, n));
                    }
                    if changes.is_empty() {
                        return Err(format!(
                            "bulk replace: anchor not found in any file matching `{path}`"
                        ));
                    }
                    report.push_str(&format!(
                        "bulk replace in `{path}`: {total} occurrence(s) across {} file(s)\n",
                        changes.len()
                    ));
                    for (f, _, _, n) in &changes {
                        report.push_str(&format!("  {f}: {n}\n"));
                    }
                    if req.dry_run {
                        report.push_str("(dry_run: not written)\n");
                    } else {
                        for (f, newtext, tgt, _) in &changes {
                            snapshot(&mut originals, &db, f);
                            db.set_text(f, newtext);
                            pending.push(DiskOp::Write(tgt.clone(), newtext.clone()));
                        }
                        report.push_str("(replaced; parse=ok all files; warm core updated)\n");
                        self.record(
                            "code_apply",
                            format!("BULK_REPLACE {path} ({} files)", changes.len()),
                        );
                    }
                    report.push('\n');
                    continue;
                }

                // ---- SUPERPOWER: move a symbol to another file (SP-5) ----
                // Cut the symbol from its source file and append it to the destination
                // (creating the destination if needed). Both files re-parsed; atomic.
                if let Some(dest) = &e.move_to {
                    let sym = target
                        .as_deref()
                        .filter(|t| !t.starts_with('@'))
                        .ok_or_else(|| {
                            format!("move needs locator `PATH#SYMBOL`: {}", e.locator)
                        })?;
                    let src_current = db
                        .text(&path)
                        .ok_or_else(|| format!("file not indexed: {path}"))?;
                    let src_syms = vyer_incr::SymbolTable {
                        symbols: db.symbols(&path).symbols.clone(),
                    };
                    let src_lang = match db.lang(&path) {
                        vyer_incr::Lang::Generic => vyer_incr::detect_lang(&path),
                        l => l,
                    };
                    let (_, span) = apply::symbol_text(&src_current, &src_syms, sym, None)
                        .map_err(|err| {
                            // SCRY-109: list the file's symbols on a failed move, matching the
                            // @into/@after/new_body errors — consistently actionable so the
                            // agent can pick the right target without a second round-trip.
                            let names: Vec<&str> =
                                src_syms.symbols.iter().map(|s| s.name.as_str()).collect();
                            if names.is_empty() {
                                format!("move failed for {path}: {err}")
                            } else {
                                format!(
                                    "move failed for {path}: {err} — symbols in {path}: {}",
                                    names.join(", ")
                                )
                            }
                        })?;
                    // SCRY-075: carry the symbol's own attributes/docs to the dest,
                    // matching what `prepare_delete` cuts from the source (so they
                    // move WITH the symbol instead of being orphaned or lost).
                    let src_lines: Vec<&str> = src_current.split_inclusive('\n').collect();
                    let tstart = apply::leading_trivia_start(
                        &src_lines,
                        (span.0 as usize).saturating_sub(1),
                        src_lang,
                    );
                    let sym_src: String =
                        src_lines[tstart..(span.1 as usize).min(src_lines.len())].concat();
                    let del =
                        apply::prepare_delete(&path, &src_current, &src_syms, src_lang, sym, None)
                            .map_err(|err| format!("move failed for {path}: {err}"))?;

                    let dest_lang = match db.lang(dest) {
                        vyer_incr::Lang::Generic => vyer_incr::detect_lang(dest),
                        l => l,
                    };
                    let new_dest = match db.text(dest) {
                        Some(t) => format!("{}\n\n{}\n", t.trim_end(), sym_src.trim_end()),
                        None => format!("{}\n", sym_src.trim_end()),
                    };
                    if vyer_index::has_parse_error(&del.new_text, src_lang)
                        || vyer_index::has_parse_error(&new_dest, dest_lang)
                    {
                        return Err("move aborted: result would not parse (source or destination); no files changed".into());
                    }
                    let src_tgt = sandbox::validate_write(&self.config.root, &path)
                        .map_err(|r| format!("move: write denied ({r:?}) for {path}"))?;
                    let dest_tgt = sandbox::validate_write(&self.config.root, dest)
                        .map_err(|r| format!("move: write denied ({r:?}) for {dest}"))?;
                    // SCRY-050: a same-file move would compute the destination append
                    // from the PRE-cut text and then collide the two writes →
                    // DUPLICATING the symbol. It's also meaningless (already there).
                    if src_tgt == dest_tgt {
                        return Err(format!(
                            "move: source and destination are the same file ({path}); a symbol can't be moved onto itself"
                        ));
                    }
                    report.push_str(&format!("move `{sym}`: {path} → {dest}\n"));
                    if req.dry_run {
                        report.push_str("(dry_run: not written)\n");
                    } else {
                        snapshot(&mut originals, &db, &path);
                        db.set_text(&path, &del.new_text);
                        pending.push(DiskOp::Write(src_tgt, del.new_text.clone()));
                        snapshot(&mut originals, &db, dest);
                        db.set_text(dest, &new_dest);
                        pending.push(DiskOp::Write(dest_tgt, new_dest.clone()));
                        report.push_str("(moved; parse=ok both files; warm core updated)\n");
                        self.record("code_apply", format!("MOVE {sym} {path} -> {dest}"));
                    }
                    report.push('\n');
                    continue;
                }

                // SECURITY: validate the write target before any disk touch (Rule §3).
                let target_path = sandbox::validate_write(&self.config.root, &path)
                    .map_err(|r| format!("write denied ({r:?}) for {path}"))?;
                // Language of the file (detect from extension when not yet indexed).
                let lang = match db.lang(&path) {
                    vyer_incr::Lang::Generic => vyer_incr::detect_lang(&path),
                    l => l,
                };

                // ---- CREATE FILE (SCRY-014): `PATH#@new` ----
                if target.as_deref() == Some("@new") {
                    let body = edit_body(e)?;
                    if db.text(&path).is_some() {
                        return Err(format!(
                            "`@new` refused: {path} already exists — use a symbol or anchor edit"
                        ));
                    }
                    if vyer_index::has_parse_error(body, lang) {
                        return Err(format!("apply failed for {path}: new file does not parse (tree-sitter found a syntax error)"));
                    }
                    report.push_str(&creation_diff(&path, body));
                    if req.dry_run {
                        report.push_str("(dry_run: not written)\n");
                    } else {
                        snapshot(&mut originals, &db, &path);
                        db.set_text(&path, body);
                        pending.push(DiskOp::Write(target_path.clone(), body.to_string()));
                        report.push_str("(created; parse=ok; warm core updated)\n");
                        self.record("code_apply", format!("CREATE {path}"));
                    }
                    report.push('\n');
                    continue;
                }

                // ---- DELETE FILE (SCRY-026): `PATH#@delete` ----
                if target.as_deref() == Some("@delete") {
                    // SCRY-115: a file can exist on disk yet be absent from the warm
                    // index (predated indexing, gitignored, or the watcher missed it).
                    // Don't refuse then — fall back to on-disk existence. Only a path
                    // in NEITHER place is a real error.
                    let in_index = db.text(&path).is_some();
                    if !in_index && !target_path.exists() {
                        return Err(format!(
                            "cannot delete {path}: not found (neither in the index nor on disk)"
                        ));
                    }
                    report.push_str(&format!("--- a/{path}\n+++ /dev/null\n"));
                    if req.dry_run {
                        report.push_str("(dry_run: not deleted)\n");
                    } else {
                        // SCRY-115: snapshot for undo — prefer the warm copy; if the
                        // file is on disk but not indexed, capture the disk bytes so
                        // `undo` can restore it.
                        if in_index {
                            snapshot(&mut originals, &db, &path);
                        } else {
                            originals
                                .entry(path.clone())
                                .or_insert_with(|| std::fs::read_to_string(&target_path).ok());
                        }
                        db.remove_text(&path);
                        pending.push(DiskOp::Delete(target_path.clone()));
                        report.push_str("(deleted; warm core updated)\n");
                        self.record("code_apply", format!("DELETE {path}"));
                    }
                    report.push('\n');
                    continue;
                }

                // Every other op needs the file's current text. SCRY-115: if it's
                // missing from the warm index but present on disk (predated indexing,
                // gitignored, or the watcher missed it), pull it in on demand instead
                // of forcing a native-tool fallback.
                if db.text(&path).is_none() {
                    if let Ok(disk) = std::fs::read_to_string(&target_path) {
                        if !disk.as_bytes().contains(&0) {
                            db.set_text(&path, &disk);
                        }
                    }
                }
                let current = db.text(&path).ok_or_else(|| {
                format!("file not found: {path} (not in the index and no such file on disk; to create it, use `{path}#@new` with new_body)")
            })?;
                let syms = vyer_incr::SymbolTable {
                    symbols: db.symbols(&path).symbols.clone(),
                };
                // SCRY-006: turn a bare "no symbol" into an actionable error by
                // listing the file's actual symbol names.
                let mkerr = |err: apply::ApplyError| -> String {
                    let hint = if matches!(err, apply::ApplyError::SymbolNotFound { .. }) {
                        let names: Vec<String> = syms
                            .symbols
                            .iter()
                            .map(|s| s.name.clone())
                            .take(15)
                            .collect();
                        if names.is_empty() {
                            String::new()
                        } else {
                            format!(" — symbols in {path}: {}", names.join(", "))
                        }
                    } else {
                        String::new()
                    };
                    format!("apply failed for {path}: {err}{hint}")
                };

                // Split an optional `@Lstart-end` off a symbol target.
                let (sym, want_range): (Option<String>, Option<(u32, u32)>) = match &target {
                    Some(t) if t.starts_with('@') => (Some(t.clone()), None),
                    Some(t) => match t.split_once("@L") {
                        Some((s, r)) => {
                            let range = r.split_once('-').and_then(|(a, b)| {
                                Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))
                            });
                            (Some(s.to_string()), range)
                        }
                        None => (Some(t.clone()), None),
                    },
                    None => (None, None),
                };

                // ---- DISPATCH ----
                let prepared = if let Some(anchor) = &e.anchor {
                    // ANCHORED edit (SCRY-004); a bare-path scope edits module-level
                    // lines/imports/constants (SCRY-002).
                    let replace = e.replace.as_deref().unwrap_or("");
                    let scope = sym.as_deref().filter(|s| !s.starts_with('@'));
                    if e.word {
                        // SCRY-046: scoped whole-word rename (safe local rename) —
                        // replace ALL whole-word `anchor` in the symbol's body.
                        prepare_scoped_word_rename(
                            &path, &current, &syms, lang, scope, want_range, anchor, replace,
                        )
                        .map_err(|err| format!("apply failed for {path}: {err}"))?
                    } else {
                        apply::prepare_anchored(
                            &path, &current, &syms, lang, scope, want_range, anchor, replace,
                        )
                        .map_err(mkerr)?
                    }
                } else if let Some(rest) = target.as_deref().and_then(|t| t.strip_prefix("@after:"))
                {
                    apply::prepare_insert(
                        &path,
                        &current,
                        &syms,
                        lang,
                        apply::InsertPos::After(rest.to_string(), None),
                        edit_body(e)?,
                    )
                    .map_err(mkerr)?
                } else if let Some(rest) =
                    target.as_deref().and_then(|t| t.strip_prefix("@before:"))
                {
                    apply::prepare_insert(
                        &path,
                        &current,
                        &syms,
                        lang,
                        apply::InsertPos::Before(rest.to_string(), None),
                        edit_body(e)?,
                    )
                    .map_err(mkerr)?
                } else if let Some(rest) =
                    target.as_deref().and_then(|t| t.strip_prefix("@delete:"))
                {
                    // DELETE a symbol's node (SCRY-026).
                    apply::prepare_delete(&path, &current, &syms, lang, rest, want_range)
                        .map_err(mkerr)?
                } else if let Some(rest) = sym.as_deref().and_then(|t| t.strip_prefix("@into:")) {
                    // INSERT INTO a container (SP-12): add a member just before the
                    // container's closing brace. Re-parse-gated like every edit.
                    // An optional `@into:Name@Lstart-end` disambiguates same-named
                    // blocks (e.g. a struct and its impl). The global `@L` split
                    // skips `@`-directives, so parse the range here.
                    let (cname, range) = match rest.split_once("@L") {
                        Some((c, r)) => (
                            c,
                            r.split_once('-').and_then(|(a, b)| {
                                Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))
                            }),
                        ),
                        None => (rest, None),
                    };
                    apply::prepare_insert(
                        &path,
                        &current,
                        &syms,
                        lang,
                        apply::InsertPos::Into(cname.to_string(), range),
                        edit_body(e)?,
                    )
                    .map_err(mkerr)?
                } else if target.as_deref() == Some("@end") {
                    apply::prepare_insert(
                        &path,
                        &current,
                        &syms,
                        lang,
                        apply::InsertPos::End,
                        edit_body(e)?,
                    )
                    .map_err(mkerr)?
                } else if let Some(s) = &sym {
                    if let Some(sym_after) = s.strip_prefix("@end:") {
                        // NEW-E: `@end` appends at end of FILE and takes no symbol;
                        // `@end:SYMBOL` is the common mistake. Point to the ops that
                        // DO take a symbol rather than a bare "unknown directive".
                        return Err(format!(
                            "`@end` takes no symbol (it appends at the end of the FILE). To add at the end of a container use `{path}#@into:{sym_after}`; for a sibling right after it use `{path}#@after:{sym_after}`."
                        ));
                    }
                    if s.starts_with('@') {
                        return Err(format!("unknown directive `#{s}` (use @after:/@before:/@into:/@end/@delete:/@new, or a symbol name)"));
                    }
                    // SCRY-081: a misplaced @-directive (`foo#@delete`) parses as a
                    // symbol named `foo#@delete` — `@` is never valid in an identifier,
                    // so guide to the correct position instead of a confusing no-edit.
                    if s.contains('@') {
                        return Err(format!(
                            "malformed locator `{}`: an @-directive goes RIGHT AFTER `#` — use `PATH#@delete:foo`, not `PATH#foo#@delete` (valid: @after:/@before:/@into:/@end/@delete:/@new)",
                            e.locator
                        ));
                    }
                    // REPLACE the whole symbol node (the original deterministic path).
                    apply::prepare_deterministic(
                        &path,
                        &current,
                        &syms,
                        lang,
                        s,
                        want_range,
                        edit_body(e)?,
                    )
                    .map_err(mkerr)?
                } else {
                    return Err(format!(
                    "locator needs a #symbol, an @directive (@after:/@before:/@into:/@end/@delete:/@new), or an anchor: {}",
                    e.locator
                ));
                };

                // SCRY-001: gate every spliced result through a real tree-sitter parse
                // so a syntactically-invalid edit can never be written with a false
                // `parse=ok` — the "no silent bad write" guarantee.
                if vyer_index::has_parse_error(&prepared.new_text, lang) {
                    return Err(format!(
                    "apply failed for {path}: edit rejected: result does not parse (tree-sitter found a syntax error)"
                ));
                }

                report.push_str(&format_diff(&prepared.diff, &path));
                if req.dry_run {
                    report.push_str("(dry_run: not written)\n");
                    self.record(
                        "code_apply",
                        format!("dry_run {path}@L{}-{}", prepared.start, prepared.end),
                    );
                } else {
                    // Update the warm core now (intra-batch freshness) but defer the
                    // disk write to the atomic commit at the end of the batch (SP-2).
                    snapshot(&mut originals, &db, &path);
                    db.set_text(&path, &prepared.new_text);
                    pending.push(DiskOp::Write(
                        target_path.clone(),
                        prepared.new_text.clone(),
                    ));
                    report.push_str("(written; parse=ok; warm core updated)\n");
                    self.record(
                        "code_apply",
                        format!("WROTE {path}@L{}-{}", prepared.start, prepared.end),
                    );
                }
                report.push('\n');
            }
            // SCRY-067 (security): the lexical sandbox (vyer-core, pure) can't see
            // that a path STRING within root may escape via a SYMLINKED directory
            // component. Before any disk write, resolve symlinks: each target's
            // longest EXISTING ancestor must canonicalize to within the canonical
            // root. An escape returns Err → the atomic rollback below (warm core
            // restored, NO file written).
            if let Ok(canon_root) = self.config.root.canonicalize() {
                for op in &pending {
                    let target = match op {
                        DiskOp::Write(p, _) => p,
                        DiskOp::Delete(p) => p,
                    };
                    let mut anc = target.as_path();
                    let existing = loop {
                        if let Ok(c) = anc.canonicalize() {
                            break Some(c);
                        }
                        match anc.parent() {
                            Some(p) if !p.as_os_str().is_empty() => anc = p,
                            _ => break None,
                        }
                    };
                    if let Some(real) = existing {
                        if !real.starts_with(&canon_root) {
                            return Err(format!(
                                "write denied: `{}` resolves outside the project root (symlinked path)",
                                target.display()
                            ));
                        }
                    }
                }
            }
            Ok(report)
        })();

        match result {
            Ok(report) => {
                // All edits validated + warm core updated: flush the buffered
                // disk writes. (Validation failures never reach here, so the
                // common abort case touched no files.)
                self.commit_pending(&mut db, &pending, &originals)?;
                // SP-6: record this batch's pre-edit state so it can be undone.
                let wrote = !pending.is_empty();
                if !originals.is_empty() {
                    self.record_history(originals.into_iter().collect());
                }
                let mut report = report;
                // SCRY-138 (guiding master): blast-radius note. When an edit REPLACES
                // a symbol's body (new_body) — the change most likely to alter
                // behavior/signature and break callers — append how many references
                // it has so the agent verifies them. The warm graph gives this for
                // free; an agent would otherwise reconstruct it by hand or skip it
                // and regress. graph=partial(approx).
                for e in &req.edits {
                    if e.new_body.is_none() {
                        continue;
                    }
                    let raw = e.locator.split(" :: ").next().unwrap_or(&e.locator).trim();
                    let Some((p, t)) = raw.split_once('#') else {
                        continue;
                    };
                    if t.starts_with('@') {
                        continue; // authoring directive, not a symbol-body replace
                    }
                    let symfull = t.split('@').next().unwrap_or(t);
                    let sym = symfull
                        .rsplit(['.', ':'])
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(symfull);
                    let Some(def_file) = self.resolve_indexed_path(&db, p) else {
                        continue;
                    };
                    let refs = self.external_refs(&db, sym, &def_file);
                    if !refs.is_empty() {
                        let shown: Vec<String> = refs
                            .iter()
                            .take(5)
                            .map(|(f, l)| format!("{f}:{l}"))
                            .collect();
                        let more = refs.len().saturating_sub(shown.len());
                        let tail = if more > 0 {
                            format!(", +{more} more")
                        } else {
                            String::new()
                        };
                        report.push_str(&format!(
                            "  blast-radius: `{sym}` has {} reference(s) (graph=partial) — verify callers still hold: {}{tail}\n",
                            refs.len(),
                            shown.join(", ")
                        ));
                    }
                }
                // SCRY-031: post-apply verify (compiles/tests, not just parses).
                if wrote {
                    if let Some(line) = self.run_verify() {
                        report.push_str(&line);
                    }
                }
                Ok(report)
            }
            Err(err) => {
                // Atomic abort: roll the warm core back to its pre-batch state.
                // No disk write happened (they were all deferred), so the repo is
                // exactly as it was before this call.
                for (p, orig) in &originals {
                    match orig {
                        Some(text) => db.set_text(p, text),
                        None => {
                            db.remove_text(p);
                        }
                    }
                }
                Err(err)
            }
        }
    }

    // ----- read-only resources: repo map + status -------------------------

    /// A PageRank "repo map": files ranked by how much the rest of the codebase
    /// depends on them, each with its top symbols, packed into a token budget.
    /// Exposed as the `vyer://repo-map` MCP Resource so an agent can orient
    /// without spending a tool call (progressive disclosure, Rule §6).
    pub fn repo_map(&self, budget_tokens: usize) -> String {
        let db = self.db.lock().unwrap();
        let files = db.files();
        let n = files.len();

        // name -> files that define it
        let mut def_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut symbols_of: Vec<Vec<String>> = vec![Vec::new(); n];
        for (i, f) in files.iter().enumerate() {
            for s in &db.symbols(f).symbols {
                if s.name.len() >= 3 {
                    def_by_name.entry(s.name.clone()).or_default().push(i);
                }
                symbols_of[i].push(s.name.clone());
            }
        }

        // edges: file i -> file d when i mentions a symbol defined in d (d != i).
        let mut edge_set: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();
        for (i, f) in files.iter().enumerate() {
            let text = match db.text(f) {
                Some(t) => t,
                None => continue,
            };
            for word in identifiers(&text) {
                if let Some(defs) = def_by_name.get(word) {
                    for &d in defs {
                        if d != i {
                            edge_set.insert((i, d));
                        }
                    }
                }
            }
        }
        let edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
        let ranks = vyer_core::repomap::pagerank(n, &edges, 0.85, 30);

        // SCRY-025c: demote unambiguously machine-GENERATED files (derived
        // artifacts an agent rarely needs to *understand*) below hand-written code
        // of similar centrality — they still appear, just lower. Orientation
        // (Rule §6) should surface the sources you actually read, not codegen.
        let eff = |i: usize| -> f64 { ranks[i] * if is_generated(&files[i]) { 0.2 } else { 1.0 } };

        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            eff(b)
                .partial_cmp(&eff(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| files[a].cmp(&files[b]))
        });

        let mut out = format!(
            "\u{27E6}vyer/repo-map v1\u{27E7} files={} edges={}\n",
            n,
            edges.len()
        );
        let mut used = budget::est_tokens(&out);
        for (rank_pos, &fi) in order.iter().enumerate() {
            let mut syms = symbols_of[fi].clone();
            // SCRY-025: a struct and its `impl` (or an overload) share a name —
            // dedup, order-preserving, so the per-file list isn't `[Locator,
            // Locator, ...]`.
            let mut seen_sym: HashSet<String> = HashSet::new();
            syms.retain(|s| seen_sym.insert(s.clone()));
            syms.truncate(6);
            let line = format!(
                "{:>3}. {}{}  rank={:.4}  [{}]\n",
                rank_pos + 1,
                // SCRY-095: sanitize the path against envelope injection (a pathological
                // filename); is_generated below still sees the real path (logic only).
                output::sanitize_field(&files[fi]),
                if is_generated(&files[fi]) {
                    " (gen)"
                } else {
                    ""
                },
                eff(fi),
                syms.join(", ")
            );
            let cost = budget::est_tokens(&line);
            if used + cost > budget_tokens {
                out.push_str("\u{27E6}more\u{27E7} lower-ranked files omitted (raise budget)\n");
                break;
            }
            used += cost;
            out.push_str(&line);
        }
        out
    }

    /// Server status as the `vyer://status` MCP Resource.
    /// SCRY-117: `vyer://project` — what an agent needs to *operate* the repo: the detected
    /// stack(s) and the real build/test/run/lint commands, parsed from the root manifests. Vyer
    /// never runs them (the host's shell does) — this tells the agent WHAT to run; it then pastes
    /// the output back into `mode=diagnose` to jump to the failures. Dynamic strings (npm script
    /// names, make targets) are envelope-sanitized (Rule §8).
    pub fn project_info(&self) -> String {
        let root = &self.config.root;
        let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
        let exists = |name: &str| root.join(name).exists();
        let mut out = String::from("\u{27E6}vyer/project v1\u{27E7}\n");
        let mut found = false;

        if exists("Cargo.toml") {
            found = true;
            out.push_str(
                "rust (cargo): build `cargo build` \u{b7} test `cargo test` \u{b7} run `cargo run` \
                 \u{b7} lint `cargo clippy` \u{b7} fmt `cargo fmt`\n",
            );
        }
        if let Some(pkg) = read("package.json") {
            found = true;
            let pm = if exists("pnpm-lock.yaml") {
                "pnpm"
            } else if exists("yarn.lock") {
                "yarn"
            } else if exists("bun.lockb") {
                "bun"
            } else {
                "npm"
            };
            out.push_str(&format!("node ({pm}): install `{pm} install`"));
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
                if let Some(scripts) = v.get("scripts").and_then(|s| s.as_object()) {
                    let run = if pm == "npm" { "npm run" } else { pm };
                    let names: Vec<String> = scripts
                        .keys()
                        .take(14)
                        .map(|k| format!("`{run} {}`", output::sanitize_field(k)))
                        .collect();
                    if !names.is_empty() {
                        out.push_str(&format!(" \u{b7} scripts: {}", names.join(" \u{b7} ")));
                    }
                }
            }
            out.push('\n');
        }
        if let Some(spec) = read("pubspec.yaml") {
            found = true;
            let t = if spec.contains("sdk: flutter") || spec.contains("\nflutter:") {
                "flutter"
            } else {
                "dart"
            };
            out.push_str(&format!(
                "dart ({t}): deps `{t} pub get` \u{b7} run `{t} run` \u{b7} test `{t} test` \
                 \u{b7} analyze `dart analyze` \u{b7} fmt `dart format .`\n"
            ));
        }
        if exists("go.mod") {
            found = true;
            out.push_str(
                "go: build `go build ./...` \u{b7} test `go test ./...` \u{b7} run `go run .` \
                 \u{b7} vet `go vet ./...`\n",
            );
        }
        if exists("pyproject.toml") || exists("setup.py") || exists("requirements.txt") {
            found = true;
            out.push_str(
                "python: test `pytest` \u{b7} install `pip install -e .` or \
                 `pip install -r requirements.txt`\n",
            );
        }
        if exists("pom.xml") {
            found = true;
            out.push_str("java (maven): build `mvn package` \u{b7} test `mvn test`\n");
        }
        if exists("build.gradle") || exists("build.gradle.kts") {
            found = true;
            out.push_str("jvm (gradle): build `./gradlew build` \u{b7} test `./gradlew test`\n");
        }
        if exists("Gemfile") {
            found = true;
            out.push_str(
                "ruby: install `bundle install` \u{b7} test `bundle exec rake` (or `rspec`)\n",
            );
        }
        if let Some(mk) = read("Makefile") {
            let targets: Vec<String> = mk
                .lines()
                .filter(|l| !l.starts_with(' ') && !l.starts_with('\t') && l.contains(':'))
                .filter_map(|l| {
                    let name = l[..l.find(':').unwrap()].trim();
                    if !name.is_empty()
                        && !name.starts_with('.')
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                    {
                        Some(format!("`make {}`", output::sanitize_field(name)))
                    } else {
                        None
                    }
                })
                .take(14)
                .collect();
            if !targets.is_empty() {
                found = true;
                out.push_str(&format!("make: {}\n", targets.join(" \u{b7} ")));
            }
        }

        // SCRY-125 (#5): monorepos keep manifests in service SUBDIRS, not the root.
        // Surface them from the indexed file set so `vyer://project` isn't empty.
        {
            const MANIFESTS: &[&str] = &[
                "Cargo.toml",
                "package.json",
                "go.mod",
                "build.gradle",
                "build.gradle.kts",
                "pom.xml",
                "pubspec.yaml",
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Gemfile",
            ];
            let db = self.db.lock().unwrap();
            let mut subs: Vec<String> = db
                .files()
                .into_iter()
                .filter(|f| {
                    f.contains('/') && MANIFESTS.iter().any(|m| f.rsplit('/').next() == Some(*m))
                })
                .collect();
            subs.sort();
            subs.dedup();
            if !subs.is_empty() {
                found = true;
                out.push_str("monorepo packages (manifests in subdirectories):\n");
                for f in subs.iter().take(40) {
                    out.push_str(&format!("  {}\n", output::sanitize_field(f)));
                }
                if subs.len() > 40 {
                    out.push_str(&format!("  … +{} more\n", subs.len() - 40));
                }
            }
        }

        if found {
            out.push_str(
                "\nVyer does NOT run these \u{2014} your host's shell does. After running a build or \
                 tests, paste the output into `code` with mode=diagnose to jump to the failures.\n",
            );
        } else {
            out.push_str("no recognized build manifest at the repo root \u{2014} inspect with detail=tree.\n");
        }
        out
    }

    pub fn status(&self) -> String {
        // SCRY-092: surface files SKIPPED at index time for exceeding max_file_bytes
        // so the agent learns a large file exists but is unindexed (honest
        // degradation, §8) instead of silently getting NO_MATCH for it.
        let skipped = self.scan_skipped_large();
        let skipped_line = if skipped.is_empty() {
            "skipped_large=0".to_string()
        } else {
            // SCRY-095: sanitize each path — a pathological filename (⟦/⟧/newline)
            // must not inject into the status envelope (same class as 088/089).
            let shown: Vec<String> = skipped
                .iter()
                .take(20)
                .map(|s| output::sanitize_field(s).into_owned())
                .collect();
            format!(
                "skipped_large(>{}B)={} [{}{}] — too large to index; use native tools for these",
                self.config.max_file_bytes,
                skipped.len(),
                shown.join(", "),
                if skipped.len() > 20 { ", …" } else { "" },
            )
        };
        // SCRY-141: surface the code_run allowlist so the agent discovers task NAMES
        // (for `code_apply {"run":"<name>"}`) without probing with an unknown task.
        let run_line = if !self.config.allow_run {
            "run=disabled (start with --allow-run)".to_string()
        } else if self.config.run_tasks.is_empty() {
            "run=enabled but no tasks (register via --run name=\"cmd\")".to_string()
        } else {
            let names: Vec<&str> = self.config.run_tasks.keys().map(String::as_str).collect();
            format!(
                "run=enabled tasks=[{}] — call code_apply {{\"run\":\"<name>\"}}",
                names.join(", ")
            )
        };
        let db = self.db.lock().unwrap();
        format!(
            "\u{27E6}vyer/status v1\u{27E7}\nroot={}\nindexed_files={}\nrevision={}\nwrites={}\nparser=tree-sitter\nlexical=ripgrep-libs\ngraph=partial(approx)\nsemantic=lexical-subword(tf-idf)\nast=tree-sitter-query\ncode.modes=auto|lexical|structural|graph|semantic|ast|diagnose\ncode.detail=locate|outline|snippet|full|refs|impact|context|count|tree|diff|ast|import|help\ncode.filters=path_scope(globs,!exclude)|lang(csv)|all_of/any_of/none_of\napply.ops=new_body|anchor/replace(+word=scoped-local-rename)|rename|move_to|@after/@before/@into/@end/@new|@delete|run|undo\nverify={}\n{}\n{}\n",
            self.config.root.display(),
            db.files().len(),
            db.revision(),
            if self.config.allow_writes { "enabled" } else { "disabled" },
            match &self.config.verify_cmd {
                Some(c) => c.join(" "),
                None => "off".to_string(),
            },
            run_line,
            skipped_line,
        )
    }

    /// SCRY-092: a metadata-only walk (no file reads) collecting files that index
    /// skips for exceeding `max_file_bytes`. Mirrors `index_repo`'s walk + dir
    /// pruning so the set matches exactly. `status` is a rare call, so the re-walk
    /// is cheaper than carrying (and invalidating) a stateful skip list.
    fn scan_skipped_large(&self) -> Vec<String> {
        let root = &self.config.root;
        let mut out = Vec::new();
        for result in ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .filter_entry(|e| !is_skippable_dir(e.file_name().to_str().unwrap_or("")))
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let too_big = path
                .metadata()
                .map(|m| m.len() as usize > self.config.max_file_bytes)
                .unwrap_or(false);
            if too_big {
                if let Some(rel) = rel_path(root, path) {
                    out.push(rel);
                }
            }
        }
        out.sort();
        out
    }
}

// ---------------------------------------------------------------------------
// small, pure helpers
// ---------------------------------------------------------------------------

/// Extract the distinct identifier-like words (len ≥ 3) from `text`. Used to
/// build the repo-map reference graph cheaply (one pass per file).
fn identifiers(text: &str) -> std::collections::HashSet<&str> {
    let mut out = std::collections::HashSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i - start >= 3 {
                out.insert(&text[start..i]);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The edit text for a replace/insert/create op: `new_body`, with an honest
/// rejection for the not-yet-enabled `lazy_edit` fast-apply path.
fn edit_body(e: &Edit) -> Result<&str, String> {
    match (&e.new_body, &e.lazy_edit) {
        (Some(b), _) => Ok(b),
        (None, Some(_)) => Err(
            "lazy_edit (fast-apply model) is a Phase-6 sidecar and not enabled; pass new_body for a deterministic splice".into(),
        ),
        (None, None) => Err(apply::ApplyError::NoEdit.to_string()),
    }
}

/// True for a syntactically valid identifier (the rename target).
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        // safe: the `&&` short-circuits on the `!s.is_empty()` above, so `next()` is Some.
        && !s.chars().next().unwrap().is_ascii_digit()
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Count whole-word call-sites of `name` (the name immediately followed, after
/// optional spaces, by `(`) in `body`. Used to detect recursion: a symbol's own
/// declaration is one `name(` call-site, so it recurses iff `name(` appears ≥2×.
fn count_call_sites(body: &str, name: &str) -> usize {
    if name.is_empty() {
        return 0;
    }
    let b = body.as_bytes();
    let nb = name.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while let Some(rel) = body[i..].find(name) {
        let p = i + rel;
        let before_ok = p == 0 || !is_ident_byte(b[p - 1]);
        let after = p + nb.len();
        let word_end = after >= b.len() || !is_ident_byte(b[after]);
        let mut j = after;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let paren = j < b.len() && b[j] == b'(';
        if before_ok && word_end && paren {
            count += 1;
        }
        i = p + nb.len();
    }
    count
}
/// 1-based line numbers where `name` occurs as a CODE identifier — skipping line
/// and block comments and string/char literals (lifetime-aware in Rust-like
/// langs), the same rules as `scan_idents`. Makes `refs` comment/string-aware
/// (SCRY-059): a name appearing only in a comment or string isn't a reference.
/// Kept separate from `scan_idents` (which many call sites depend on) so that
/// critical path stays untouched; the skip logic is intentionally mirrored.
fn code_ident_lines(text: &str, name: &str, lang: vyer_incr::Lang) -> HashSet<u32> {
    // SCRY-113: derive every string-syntax rule from one `lang` (was 4 bool params).
    let sq_is_string = sq_is_string_lang(lang);
    let hash_comment = hash_is_comment_lang(lang);
    let backtick_string = backtick_is_string_lang(lang);
    let triple_quote = triple_quote_lang(lang);
    let raw_string = raw_string_lang(lang);
    let mut line_starts = vec![0usize];
    for (idx, c) in text.bytes().enumerate() {
        if c == b'\n' {
            line_starts.push(idx + 1);
        }
    }
    let b = text.as_bytes();
    let mut out: HashSet<u32> = HashSet::new();
    let mut i = 0;
    let mut cur_start: Option<usize> = None;
    let mut flush = |start: usize, end: usize| {
        if &text[start..end] == name {
            out.insert(line_starts.partition_point(|&s| s <= start) as u32);
        }
    };
    while i < b.len() {
        if (b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/') || (hash_comment && b[i] == b'#') {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        // SCRY-112/113: Rust raw string `r"..."`/`r#"..."#` — skip to the hash-balanced close.
        if raw_string && b[i] == b'r' && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                if let Some(s) = cur_start.take() {
                    flush(s, i);
                }
                i = j + 1;
                while i < b.len() {
                    if b[i] == b'"' {
                        let mut k = i + 1;
                        let mut h = 0usize;
                        while h < hashes && k < b.len() && b[k] == b'#' {
                            h += 1;
                            k += 1;
                        }
                        if h == hashes {
                            i = k;
                            break;
                        }
                    }
                    i += 1;
                }
                continue;
            }
        }
        // SCRY-111: triple-quoted string (Python/Dart `"""`/`'''`) — skip to close.
        if triple_quote
            && i + 2 < b.len()
            && (b[i] == b'"' || b[i] == b'\'')
            && b[i + 1] == b[i]
            && b[i + 2] == b[i]
        {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            let q = b[i];
            i += 3;
            while i + 2 < b.len() && !(b[i] == q && b[i + 1] == q && b[i + 2] == q) {
                i += 1;
            }
            i = (i + 3).min(b.len());
            continue;
        }
        if b[i] == b'"' || (b[i] == b'\'' && sq_is_string) {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            let q = b[i];
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            continue;
        }
        // SCRY-110: backtick string (JS/TS template literal, Go raw string) — skip, so
        // a symbol mention in template/raw-string text isn't counted as a reference.
        if b[i] == b'`' && backtick_string {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            i += 1;
            while i < b.len() && b[i] != b'`' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            continue;
        }
        if b[i] == b'\'' {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            let is_char = if i + 1 < b.len() && b[i + 1] == b'\\' {
                i + 3 < b.len() && b[i + 3] == b'\''
            } else {
                i + 2 < b.len() && b[i + 1] != b'\'' && b[i + 2] == b'\''
            };
            if is_char {
                i += if b.get(i + 1) == Some(&b'\\') { 4 } else { 3 };
                i = i.min(b.len());
            } else {
                i += 1;
            }
            continue;
        }
        let c = b[i];
        if c.is_ascii_alphanumeric() || c == b'_' {
            if cur_start.is_none() && (c.is_ascii_alphabetic() || c == b'_') {
                cur_start = Some(i);
            }
            i += 1;
        } else {
            if let Some(s) = cur_start.take() {
                flush(s, i);
            }
            i += 1;
        }
    }
    if let Some(s) = cur_start.take() {
        flush(s, b.len());
    }
    out
}
/// Scan `body` for identifiers, skipping line/block comments and string/char
/// literals (SCRY-043). With `calls_only`, keep only identifiers in CALL
/// position (name immediately followed by `(`) — the precise basis for a
/// `detail=context` `[calls]` list. Without it, keep every code identifier —
/// used for caller/reference detection so a name mentioned only in a comment or
/// string does not count as a real reference.
fn scan_idents(body: &str, calls_only: bool, lang: vyer_incr::Lang) -> HashSet<String> {
    // SCRY-113: derive every string-syntax rule from one `lang` (was 4 bool params).
    let sq_is_string = sq_is_string_lang(lang);
    let hash_comment = hash_is_comment_lang(lang);
    let backtick_string = backtick_is_string_lang(lang);
    let triple_quote = triple_quote_lang(lang);
    let raw_string = raw_string_lang(lang);
    let mut out = HashSet::new();
    let b = body.as_bytes();
    let mut i = 0;
    let mut cur_start: Option<usize> = None;
    // Flush a just-ended identifier [start,i): insert it unless we only want
    // call-sites and the next non-blank byte isn't `(`.
    let flush = |out: &mut HashSet<String>, start: usize, i: usize| {
        let name = &body[start..i];
        if calls_only {
            let mut j = i;
            let skip_ws = |mut k: usize| {
                while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                    k += 1;
                }
                k
            };
            j = skip_ws(j);
            // Skip a Rust turbofish `::<...>` so `foo::<T>()` still counts `foo`
            // as a call (balanced angle brackets handle nested generics).
            if j + 1 < b.len() && b[j] == b':' && b[j + 1] == b':' {
                let mut k = skip_ws(j + 2);
                if k < b.len() && b[k] == b'<' {
                    let mut depth = 0i32;
                    while k < b.len() {
                        match b[k] {
                            b'<' => depth += 1,
                            b'>' => {
                                depth -= 1;
                                if depth == 0 {
                                    k += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    j = skip_ws(k);
                }
            }
            if !(j < b.len() && b[j] == b'(') {
                return;
            }
        }
        out.insert(name.to_string());
    };
    while i < b.len() {
        // line comment (`//`, or `#` in Python/Ruby — SCRY-060)
        if (b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/') || (hash_comment && b[i] == b'#') {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // block comment
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        // SCRY-112/113: Rust raw string `r"..."`/`r#"..."#` — skip to the hash-balanced
        // close so a name mentioned only inside it isn't counted as a reference.
        if raw_string && b[i] == b'r' && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                if let Some(s) = cur_start.take() {
                    flush(&mut out, s, i);
                }
                i = j + 1;
                while i < b.len() {
                    if b[i] == b'"' {
                        let mut k = i + 1;
                        let mut h = 0usize;
                        while h < hashes && k < b.len() && b[k] == b'#' {
                            h += 1;
                            k += 1;
                        }
                        if h == hashes {
                            i = k;
                            break;
                        }
                    }
                    i += 1;
                }
                continue;
            }
        }
        // SCRY-111: triple-quoted string (Python/Dart `"""`/`'''`) — skip to the
        // closing triple, BEFORE the single `"` handler.
        if triple_quote
            && i + 2 < b.len()
            && (b[i] == b'"' || b[i] == b'\'')
            && b[i + 1] == b[i]
            && b[i + 2] == b[i]
        {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            let q = b[i];
            i += 3;
            while i + 2 < b.len() && !(b[i] == q && b[i + 1] == q && b[i + 2] == q) {
                i += 1;
            }
            i = (i + 3).min(b.len());
            continue;
        }
        // Double-quoted string (every language), or single-quoted string in
        // languages where `'` delimits strings (Python/JS/TS/Dart/PHP/Ruby).
        if b[i] == b'"' || (b[i] == b'\'' && sq_is_string) {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            let q = b[i];
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            continue;
        }
        // SCRY-110: backtick string (JS/TS template literal, Go raw string) — skip, so
        // a name mentioned only in template/raw-string text isn't a false reference.
        if b[i] == b'`' && backtick_string {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            i += 1;
            while i < b.len() && b[i] != b'`' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            continue;
        }
        // In Rust-like languages `'` is a CHAR literal (`'a'`, `'\n'`) OR a
        // LIFETIME (`'a`, `'static`). Skip a well-formed char literal; treat a
        // lifetime as a bare tick so the surrounding identifiers/calls are NOT
        // swallowed — the bug a naive "scan to the next quote" causes for
        // lifetimed Rust (it would eat every call between two ticks).
        if b[i] == b'\'' {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            let is_char = if i + 1 < b.len() && b[i + 1] == b'\\' {
                i + 3 < b.len() && b[i + 3] == b'\''
            } else {
                i + 2 < b.len() && b[i + 1] != b'\'' && b[i + 2] == b'\''
            };
            if is_char {
                i += if b.get(i + 1) == Some(&b'\\') { 4 } else { 3 };
                i = i.min(b.len());
            } else {
                i += 1; // bare lifetime tick
            }
            continue;
        }
        let c = b[i];
        if c.is_ascii_alphanumeric() || c == b'_' {
            if cur_start.is_none() && (c.is_ascii_alphabetic() || c == b'_') {
                cur_start = Some(i);
            }
            i += 1;
        } else {
            if let Some(s) = cur_start.take() {
                flush(&mut out, s, i);
            }
            i += 1;
        }
    }
    if let Some(s) = cur_start.take() {
        flush(&mut out, s, b.len());
    }
    out
}

/// Does `'` delimit a STRING in this language (vs. a char literal / lifetime)?
/// Python/JS/TS/Dart/PHP/Ruby: yes. Rust/Go/Java/Kotlin/Swift/C/C++/C#: no (it's
/// a char literal or, in Rust, a lifetime). Unknown (`Generic`): assume yes —
/// plain/unknown text is far more likely to quote with `'` than to be lifetimed
/// systems code. Drives `scan_idents` so lifetimes don't eat surrounding calls.
fn sq_is_string_lang(lang: vyer_incr::Lang) -> bool {
    use vyer_incr::Lang;
    !matches!(
        lang,
        Lang::Rust
            | Lang::Go
            | Lang::Java
            | Lang::Kotlin
            | Lang::Swift
            | Lang::C
            | Lang::Cpp
            | Lang::CSharp
    )
}

/// True where `#` starts a line comment (Python/Ruby/shell-family). In Rust it's
/// an attribute (`#[derive]`) and in JS/TS a private field (`#x`), so the comment
/// skip in the identifier scanners is gated on this (SCRY-060).
fn hash_is_comment_lang(lang: vyer_incr::Lang) -> bool {
    use vyer_incr::Lang;
    matches!(lang, Lang::Python | Lang::Ruby)
}

/// SCRY-110: True where a backtick (`` ` ``) starts a STRING — JS/TS template
/// literals and Go raw strings. The identifier scanners must skip these so a symbol
/// name inside a template/raw string isn't (silently) renamed or counted as a
/// reference. In other languages a backtick isn't a string delimiter.
fn backtick_is_string_lang(lang: vyer_incr::Lang) -> bool {
    use vyer_incr::Lang;
    matches!(
        lang,
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Go
    )
}

/// SCRY-111: True where `"""`/`'''` start a TRIPLE-QUOTED string — Python docstrings
/// and Dart multi-line strings. Handled explicitly so the scanners don't rely on the
/// fragile sequential quote-pairing, which desyncs on an ODD number of internal
/// quotes inside the docstring (e.g. a `5"` measurement), wrongly treating a trailing
/// symbol mention as code. (NOTE: the scanners now take 4 `*_lang` bools; a future
/// cleanup would pass `lang` once and derive them internally — SCRY-111 follow-up.)
fn triple_quote_lang(lang: vyer_incr::Lang) -> bool {
    use vyer_incr::Lang;
    matches!(lang, Lang::Python | Lang::Dart)
}

/// SCRY-112: True where `r"..."` / `r#"..."#` is a RAW string — Rust only. Raw strings
/// hold internal `"` verbatim, so the scanners must skip to the hash-balanced close.
fn raw_string_lang(lang: vyer_incr::Lang) -> bool {
    matches!(lang, vyer_incr::Lang::Rust)
}
/// Split text into lowercase subword tokens, breaking on non-alphanumerics AND
/// camelCase boundaries (so `validateToken` and `validate_token` both yield
/// ["validate", "token"]). The basis of semantic-subword retrieval (SP-7).
fn subword_tokens(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                toks.push(std::mem::take(&mut cur));
            }
            cur.push(c.to_ascii_lowercase());
            prev_lower = c.is_lowercase();
        } else {
            if !cur.is_empty() {
                toks.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks.retain(|t| t.len() >= 2); // drop noise single chars
    toks
}

/// Reserved keywords across vyer's tier-1 languages — names a rename must not
/// produce (tree-sitter accepts a keyword in an identifier slot via error
/// recovery, but the real compiler would reject it).
fn is_language_keyword(name: &str) -> bool {
    const KW: &[&str] = &[
        // Rust
        "fn",
        "let",
        "mut",
        "struct",
        "enum",
        "trait",
        "impl",
        "mod",
        "pub",
        "use",
        "match",
        "if",
        "else",
        "for",
        "while",
        "loop",
        "return",
        "self",
        "Self",
        "where",
        "type",
        "const",
        "static",
        "ref",
        "move",
        "dyn",
        "as",
        "in",
        "unsafe",
        "async",
        "await",
        "crate",
        "super",
        // Python
        "def",
        "class",
        "lambda",
        "pass",
        "yield",
        "import",
        "from",
        "global",
        "nonlocal",
        "with",
        "try",
        "except",
        "finally",
        "raise",
        "assert",
        "del",
        "elif",
        "is",
        "not",
        "and",
        "or",
        "None",
        "True",
        "False",
        // JS/TS extras
        "function",
        "var",
        "new",
        "delete",
        "typeof",
        "instanceof",
        "void",
        "this",
        "extends",
        "interface",
        "export",
        "default",
        "case",
        "switch",
        "break",
        "continue",
        "throw",
        "catch",
    ];
    KW.contains(&name)
}

/// Literal replace-all of `anchor` with `replace`, returning the count (SP-4).
fn replace_all_str(text: &str, anchor: &str, replace: &str) -> (String, usize) {
    if anchor.is_empty() {
        return (text.to_string(), 0);
    }
    (text.replace(anchor, replace), text.matches(anchor).count())
}

/// Replace every *whole-word* occurrence of `old` with `new` (identifier
/// boundaries), returning the new text and the replacement count. This is the
/// same word-boundary approximation the `refs` graph uses — honest, language-
/// agnostic, and validated by re-parse at the call site (SCRY-027).
/// SCRY-074: whole-word replace of `old`→`new`, but ONLY in CODE — occurrences
/// inside strings, char literals, and comments are left untouched, so a rename
/// never corrupts string DATA (`"foo should not change"`) or comment prose. The
/// string/comment state machine mirrors `scan_idents` (the graph already excludes
/// these, so rename is now consistent with refs/context). `sq_is_string` /
/// `hash_comment` are the per-language gates.
fn replace_word(text: &str, old: &str, new: &str, lang: vyer_incr::Lang) -> (String, usize) {
    if old.is_empty() {
        return (text.to_string(), 0);
    }
    // SCRY-113: one `lang` arg → all string-syntax rules derived here (was 5 bool params
    // threaded through every call site). A new string syntax is added in ONE place.
    let sq_is_string = sq_is_string_lang(lang);
    let hash_comment = hash_is_comment_lang(lang);
    let backtick_string = backtick_is_string_lang(lang);
    let triple_quote = triple_quote_lang(lang);
    let raw_string = raw_string_lang(lang);
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        // line comment (`//`, or `#` in Python/Ruby) — copy verbatim to EOL.
        if (b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/') || (hash_comment && b[i] == b'#') {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        // block comment — copy verbatim.
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            out.push_str(&text[start..i]);
            continue;
        }
        // SCRY-112: Rust raw string `r"..."` / `r#"..."#` (N hashes) — copy verbatim to
        // the matching close (`"` + the same hash count). Raw strings exist precisely to
        // hold internal `"`, so the normal string handler would end early and rename a
        // symbol mentioned inside. `r` must not be mid-identifier.
        if raw_string && b[i] == b'r' && (i == 0 || !is_ident_byte(b[i - 1])) {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                let start = i;
                i = j + 1;
                while i < b.len() {
                    if b[i] == b'"' {
                        let mut k = i + 1;
                        let mut h = 0usize;
                        while h < hashes && k < b.len() && b[k] == b'#' {
                            h += 1;
                            k += 1;
                        }
                        if h == hashes {
                            i = k;
                            break;
                        }
                    }
                    i += 1;
                }
                out.push_str(&text[start..i]);
                continue;
            }
        }
        // SCRY-111: triple-quoted string (Python/Dart `"""`/`'''`) — copy verbatim to
        // the closing triple, BEFORE the single `"` handler, so internal quotes don't
        // desync the quote-pairing and a symbol in a docstring isn't renamed.
        if triple_quote
            && i + 2 < b.len()
            && (b[i] == b'"' || b[i] == b'\'')
            && b[i + 1] == b[i]
            && b[i + 2] == b[i]
        {
            let start = i;
            let q = b[i];
            i += 3;
            while i + 2 < b.len() && !(b[i] == q && b[i + 1] == q && b[i + 2] == q) {
                i += 1;
            }
            i = (i + 3).min(b.len());
            out.push_str(&text[start..i]);
            continue;
        }
        // string: double-quote always; single-quote where it delimits strings.
        if b[i] == b'"' || (b[i] == b'\'' && sq_is_string) {
            let start = i;
            let q = b[i];
            i += 1;
            while i < b.len() && b[i] != q {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&text[start..i]);
            continue;
        }
        // SCRY-110: backtick string — JS/TS template literal or Go raw string.
        // Copy verbatim to EOS so a symbol name in the string TEXT isn't silently
        // corrupted by a rename. No `\` un-escaping (Go raw strings have none; a
        // `${}` interpolation is treated as string text too — a rename inside it is
        // skipped, recoverable, vs the silent data corruption of editing string text).
        if b[i] == b'`' && backtick_string {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'`' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&text[start..i]);
            continue;
        }
        // Rust-like `'`: a char literal or a lifetime — copy verbatim (skip a
        // well-formed char; treat a lifetime as a bare tick so the following
        // identifier is still scanned as code).
        if b[i] == b'\'' {
            let start = i;
            let is_char = if i + 1 < b.len() && b[i + 1] == b'\\' {
                i + 3 < b.len() && b[i + 3] == b'\''
            } else {
                i + 2 < b.len() && b[i + 1] != b'\'' && b[i + 2] == b'\''
            };
            if is_char {
                i += if b.get(i + 1) == Some(&b'\\') { 4 } else { 3 };
                i = i.min(b.len());
            } else {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        // CODE: a whole-word match of `old`.
        if b[i..].starts_with(old.as_bytes()) {
            let before_ok = i == 0 || !is_ident_byte(b[i - 1]);
            let j = i + old.len();
            let after_ok = j >= b.len() || !is_ident_byte(b[j]);
            if before_ok && after_ok {
                out.push_str(new);
                count += 1;
                i = j;
                continue;
            }
        }
        // copy one UTF-8 char (i is always on a char boundary here)
        let ch = text[i..].chars().next().unwrap();
        let l = ch.len_utf8();
        out.push_str(&text[i..i + l]);
        i += l;
    }
    (out, count)
}

/// SCRY-046: rename every whole-word occurrence of `anchor` within the locator's
/// symbol scope (or the whole file if no symbol) to `replace`, validated by a
/// real re-parse. The safe local-variable rename — scoped, whole-word, all-occur.
#[allow(clippy::too_many_arguments)]
fn prepare_scoped_word_rename(
    path: &str,
    current: &str,
    syms: &vyer_incr::SymbolTable,
    lang: vyer_incr::Lang,
    scope: Option<&str>,
    want_range: Option<(u32, u32)>,
    anchor: &str,
    replace: &str,
) -> Result<apply::PreparedEdit, String> {
    if anchor.is_empty() {
        return Err("word rename needs a non-empty anchor (the token to rename)".into());
    }
    let lines: Vec<&str> = current.split_inclusive('\n').collect();
    let (lo, hi) = match scope {
        Some(name) => {
            let s = syms
                .symbols
                .iter()
                .filter(|s| s.name == name)
                .find(|s| match want_range {
                    Some((a, b)) => s.start <= b && s.end >= a,
                    None => true,
                })
                .ok_or_else(|| format!("no symbol `{name}` in {path} to scope the rename"))?;
            (s.start as usize, s.end as usize)
        }
        None => (1, lines.len()),
    };
    let lo0 = lo.saturating_sub(1).min(lines.len());
    let hi0 = hi.min(lines.len());
    let scope_text: String = lines[lo0..hi0].concat();
    let (new_scope, n) = replace_word(&scope_text, anchor, replace, lang);
    if n == 0 {
        return Err(format!(
            "word rename: whole-word `{anchor}` not found in the scope"
        ));
    }
    let mut new_text = String::with_capacity(current.len() + n * replace.len());
    new_text.push_str(&lines[..lo0].concat());
    new_text.push_str(&new_scope);
    new_text.push_str(&lines[hi0..].concat());
    if vyer_index::has_parse_error(&new_text, lang) {
        return Err(format!(
            "word rename `{anchor}`→`{replace}` would not parse; no change"
        ));
    }
    let diff = apply::line_diff(path, current, &new_text);
    Ok(apply::PreparedEdit {
        new_text,
        diff,
        start: lo as u32,
        end: hi as u32,
    })
}

/// A deferred filesystem mutation (SP-2 atomic apply). Disk writes are buffered
/// during a `code_apply` batch and only flushed if every edit validated — so a
/// failure partway through touches no files.
enum DiskOp {
    Write(std::path::PathBuf, String),
    Delete(std::path::PathBuf),
}

/// SCRY-102: true if `text` uses CRLF line endings predominantly (a Windows file).
/// Conservative — `lf` counts every `\n` (including those in `\r\n`), so this is true
/// only when CRLF strictly outnumbers lone LF; an LF file (crlf=0) is always false.
fn is_crlf_dominant(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    crlf > 0 && 2 * crlf > lf
}

/// SCRY-102: normalize all line endings to CRLF. Idempotent — existing `\r\n`
/// round-trips unchanged (collapse to `\n`, then expand), so untouched lines are
/// byte-identical and only spliced-in `\n` becomes `\r\n`.
fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Record a touched file's pre-batch text once, so the warm core can be rolled
/// back if a later edit in the same batch fails (SP-2). `None` == didn't exist.
fn snapshot(
    originals: &mut std::collections::HashMap<String, Option<String>>,
    db: &Db,
    path: &str,
) {
    originals
        .entry(path.to_string())
        .or_insert_with(|| db.text(path).map(|s| s.to_string()));
}

/// A unified diff for a freshly-created file (SCRY-014).
fn creation_diff(path: &str, body: &str) -> String {
    // SCRY-037: don't echo the whole new file back — the agent just authored it.
    // A header + a short head preview keeps the create response compact.
    let n = body.lines().count();
    const HEAD: usize = 4;
    let head: Vec<&str> = body.lines().take(HEAD).collect();
    let mut out = format!("new file {path} (+{n} lines)\n```\n{}\n", head.join("\n"));
    if n > head.len() {
        out.push_str(&format!("... {} more lines\n", n - head.len()));
    }
    out.push_str("```\n");
    out
}

/// Wrap a unified diff with a one-line `+N -M lines (path)` summary and a fenced
/// ```diff block so markdown-aware clients colorize the +/- lines (SCRY-036).
/// MCP can't trigger Claude Code's native diff widget (reserved for its built-in
/// Edit/Write tools), so this is the most readable form vyer can return.
fn format_diff(diff: &str, path: &str) -> String {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    // Compact output (vyer's core principle): cap a very large diff so the apply
    // response never floods the agent's context (SCRY-037).
    const CAP: usize = 60;
    let lines: Vec<&str> = diff.trim_end().lines().collect();
    let body = if lines.len() > CAP {
        format!(
            "{}\n... {} more diff lines (truncated)",
            lines[..CAP].join("\n"),
            lines.len() - CAP
        )
    } else {
        lines.join("\n")
    };
    format!(
        "+{added} -{removed} lines ({})\n```diff\n{body}\n```\n",
        output::sanitize_field(path)
    )
}

/// Escape any regex metacharacters in an identifier-ish query so the `\bX\b`
/// reference pattern stays well-formed even for odd inputs.
fn regex_escape_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !(c.is_alphanumeric() || c == '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether a query is a single plain identifier (so the token index can prune).
fn is_plain_ident(q: &str) -> bool {
    let q = q.trim();
    q.len() >= 3
        && q.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && q.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

/// SCRY-119: accept a fully-qualified locator `PATH#SYMBOL[@Lx-y]` as a graph
/// query target (the disambiguated form the playbook recommends), not just a bare
/// `SYMBOL`. Returns (optional path scope, symbol name). Without a `#`, the whole
/// string is the name — unchanged behavior.
fn parse_symbol_query(q: &str) -> (Option<String>, String) {
    let q = q.trim();
    let (path, sym) = match q.rsplit_once('#') {
        Some((p, s)) if !p.trim().is_empty() && !s.trim().is_empty() => {
            (Some(p.trim().to_string()), s.trim())
        }
        _ => (None, q),
    };
    // Drop a trailing `@Lstart-end` range and any `:: blake3=…` suffix.
    let sym = sym.split("@L").next().unwrap_or(sym);
    let sym = sym.split(" :: ").next().unwrap_or(sym);
    (path, sym.trim().to_string())
}

/// SCRY-119: two languages a symbol rename may legitimately span. Same language
/// always matches; the JS/TS/TSX family is one (a symbol can move between them);
/// every other language is confined to itself — so renaming a Python class never
/// rewrites a same-named TypeScript interface or prose in a `.md`.
fn same_lang_family(a: vyer_incr::Lang, b: vyer_incr::Lang) -> bool {
    use vyer_incr::Lang::{JavaScript, Tsx, TypeScript};
    if a == b {
        return true;
    }
    let web = |l: vyer_incr::Lang| matches!(l, JavaScript | TypeScript | Tsx);
    web(a) && web(b)
}

/// SCRY-047: identifier tokens (≥3 chars, lowercased) of a PURE LITERAL phrase
/// query — e.g. `fn parse` → `["parse"]`. Returns `None` for a query containing
/// regex metacharacters, where an AND-prune would be unsound (e.g. `a|b` matches
/// with only one token present). When `Some`, every match of the literal must
/// contain every token, so we can prune the lexical scan to files that hold them
/// all — turning a multi-word phrase from "scan every file" into an index lookup.
fn literal_phrase_tokens(q: &str) -> Option<Vec<String>> {
    // Only metacharacters that can make an extracted identifier token OPTIONAL
    // disqualify a prune. `.` (any single char), `^`/`$` (anchors) keep every
    // token required, so `obj.method` still prunes to files holding BOTH tokens
    // (a superset of the adjacency the regex needs — sound). `? * + | ( ) [ ] { }
    // \\` can drop or alternate a token, so they fall through to a full scan.
    const OPTIONAL_META: &[char] = &['+', '*', '?', '(', ')', '[', ']', '{', '}', '|', '\\'];
    if q.chars().any(|c| OPTIONAL_META.contains(&c)) {
        return None;
    }
    let mut toks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, toks: &mut Vec<String>| {
        if cur.len() >= 3
            && cur
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            toks.push(cur.to_ascii_lowercase());
        }
        cur.clear();
    };
    for c in q.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut toks);
        }
    }
    flush(&mut cur, &mut toks);
    if toks.is_empty() {
        None
    } else {
        Some(toks)
    }
}

/// SCRY-065: the required LITERAL PREFIX of a regex, if any. A regex that begins
/// with a literal run (`validate_\w+`, `error.*handler`, `^foo.*`) only matches
/// text that BEGINS with that run, so every matching file must contain it — sound
/// to prune by. The run's last char is dropped when a `?`/`*`/`{` quantifies it
/// optional (`abc?def` → `ab`); `+` (one-or-more) keeps it. Returns None if the
/// pattern starts with a metachar/anchor or the required run is <3 chars.
/// Lowercased for the (lowercased) token index. Only reached for patterns
/// `literal_phrase_tokens` rejected (i.e. genuine regexes).
fn regex_required_prefix(pattern: &str) -> Option<String> {
    let s = pattern.strip_prefix('^').unwrap_or(pattern);
    let b = s.as_bytes();
    // A TOP-LEVEL alternation (`|` at paren-depth 0) means the leading run is just
    // ONE branch — a match could take the other branch and not contain it. So the
    // prefix is NOT required: bail (full scan). (Escaped `\|`/`\(` are skipped.)
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1, // skip the escaped char
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'|' if depth <= 0 => return None,
            _ => {}
        }
        i += 1;
    }
    let mut end = 0;
    while end < b.len() && (b[end].is_ascii_alphanumeric() || b[end] == b'_') {
        end += 1;
    }
    if end == 0 {
        return None; // starts with a metachar / anchor
    }
    // A `?`/`*`/`{...}` immediately after the run makes the run's LAST char
    // optional, so it isn't required; `+` (one-or-more) keeps it.
    let run_len = if end < b.len() && matches!(b[end], b'?' | b'*' | b'{') {
        end - 1
    } else {
        end
    };
    let run = &s[..run_len];
    if run.len() >= 3
        && run
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        Some(run.to_ascii_lowercase())
    } else {
        None
    }
}

/// SCRY-066: ALL required literal runs of a FLAT regex — one with no group `(`,
/// alternation `|`, char-class `[`, or counted quantifier `{` (whose contents are
/// NOT literal text: `[abc]` is one-OF, `b{100}` is a count). A flat regex is a
/// linear sequence of literal runs separated by `.`/`*`/`+`/`?`/`\`/anchors, so
/// every run NOT made optional by a trailing `?`/`*` appears in EVERY match —
/// AND-prune by all of them (more selective than the prefix alone). Returns empty
/// for non-flat patterns (the caller falls back to the prefix-only path).
fn regex_required_literals(pattern: &str) -> Vec<String> {
    let s0 = pattern.strip_prefix('^').unwrap_or(pattern);
    let s = s0.strip_suffix('$').unwrap_or(s0);
    let b = s.as_bytes();
    if b.iter().any(|&c| matches!(c, b'(' | b'|' | b'[' | b'{')) {
        return Vec::new(); // not flat — those constructs' contents aren't literals
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;
    while i <= b.len() {
        let c = if i < b.len() { Some(b[i]) } else { None };
        let is_word = matches!(c, Some(ch) if ch.is_ascii_alphanumeric() || ch == b'_');
        if is_word {
            if start.is_none() {
                start = Some(i);
            }
            i += 1;
            continue;
        }
        // the run (if any) ends at i; a trailing `?`/`*` makes its LAST char optional.
        if let Some(st) = start.take() {
            let drop_last = matches!(c, Some(b'?') | Some(b'*'));
            let end = if drop_last { i - 1 } else { i };
            if end > st {
                let run = &s[st..end];
                if run.len() >= 3
                    && run
                        .chars()
                        .next()
                        .is_some_and(|x| x.is_ascii_alphabetic() || x == '_')
                {
                    out.push(run.to_ascii_lowercase());
                }
            }
        }
        // an escape (`\w`, `\.`) consumes the next char too — never a literal run.
        i += if c == Some(b'\\') { 2 } else { 1 };
    }
    out
}

/// SCRY-041: a recovery hint tailored to the query that found nothing, instead
/// of one generic string. Looks at the last query's shape — multi-word vs a
/// single symbol, a restrictive/excluding `path_scope`, a `lang` filter — and
/// suggests the most likely fix first, so an agent recovers in one step.
/// The `detail=help` capability sheet (SCRY-132). Authoritative, compact, and
/// drift-guarded by `help_lists_every_mode_and_detail` — if a mode/detail is
/// added without listing it here, that test fails. This is the schema-as-truth
/// surface every agent report asked for.
const CODE_HELP: &str = "\
⟦vyer help⟧ one tool `code` (search/read/navigate) + gated `code_apply` (edit).
INPUT SHAPE — code accepts ANY of:
  • a single query object:   {\"q\":\"validateToken\",\"detail\":\"snippet\"}
  • a bare string:           \"validateToken\"
  • a batch:                 {\"queries\":[{\"q\":\"a\"},\"b\"],\"budget_tokens\":8000}
  (no need to wrap one query in queries:[…]; each batch item may be a bare string.)

QUERY FIELDS:
  q          search text / identifier / NL / AST pattern (per mode). Optional if `path` set.
  path       read a file by repo-relative path (the Read replacement). With `lines`.
  lines      range for `path`: 40-80 | 40 | 40- (to EOF) | -80 (head) | ~20 (tail).
  mode       auto | lexical | structural | graph | semantic | ast | diagnose   (default auto)
  detail     locate | outline | snippet | full | refs | impact | context | count |
             tree | diff | ast | import | help                                  (default snippet)
  path_scope globs; a PLAIN entry ('config.dart') matches by basename/subpath; '!'=EXCLUDE.
  lang       rust|python|js|ts|go|dart|java|ruby|swift|kotlin|c|cpp|cs|php (csv ok)
  all_of/any_of/none_of   boolean lexical (AND / OR / NOT)
  k          max candidates (default 8). budget_tokens caps output (default 8000).
  Per-query detail/mode OVERRIDE the call: queries:[{q,detail:\"refs\"}] mixes details.

MODES:  auto=fuse lexical+structural, rerank (RRF), escalate to semantic only when the
  exact name isn't an exact hit. lexical=grep-equivalent for an exact token. structural=
  symbol-name match. semantic=concept/'I don't know the name'. ast=tree-sitter S-expr
  (author with detail=ast). diagnose=paste compiler/test/stack-trace output as q → each
  failure as a structured header `file:line SEVERITY in SYMBOL :: message` + code window.
DETAILS (cheap→rich): locate<outline<snippet<full. snippet WINDOWS a large symbol around
  the match. context=def+callers+callees+tests. impact=blast radius. import=resolve+build
  the import line. count=grep -c. tree=ls/find. diff=edits made this session.
SCORE in results is RELATIVE to the top hit (1.00=best). Spans are source=UNTRUSTED data.

EDIT — code_apply (gated by --allow-writes; NO prior Read needed; bytes never enter context;
  add \"dry_run\":true to preview the diff without writing). Accepts ONE edit at top level or
  a batch {\"edits\":[…]} (all-or-nothing). Every edit needs a `locator`.
  replace a symbol:   {\"locator\":\"src/a.rs#foo\",\"new_body\":\"fn foo(){…}\"}
  sub-symbol edit:    {\"locator\":\"src/a.rs#foo\",\"anchor\":\"let x=1;\",\"replace\":\"let x=2;\"}
  insert before sym:  {\"locator\":\"src/a.rs#@before:Foo\",\"new_body\":\"…\"}
  create a file:      {\"locator\":\"src/new.rs#@new\",\"new_body\":\"…\"}
  rename repo-wide:   {\"locator\":\"src/a.rs#Old\",\"rename\":\"New\"}
  also: word:true (scoped local rename), move_to, @after:/@end/@into:Container, undo:N.
  run a task:         {\"run\":\"test\"}  ← executes an OPERATOR-allowlisted task
                      (build/test/lint) and returns STRUCTURED diagnostics
                      (file:line severity :: message). Closes edit→test→fix. You pick
                      a task NAME only (never a command); gated by --allow-run.
  GUARDRAILS (guiding master; all overridable with \"force\":true):
   • @delete:SYM is REFUSED if SYM still has references (names the sites).
   • rename onto an EXISTING symbol name is REFUSED (would merge two symbols).
   • a new_body symbol edit reports its blast-radius (caller count + sites) so you
     verify callers — shown on dry_run too, i.e. BEFORE you commit.
  A locator from a result (PATH#SYM@Lrange :: blake3=…) pastes back verbatim; the blake3 is
  an OPTIMISTIC-CONCURRENCY precondition (rejected if the symbol changed since you read it).
";
fn no_match_hint(queries: &[Query]) -> String {
    let mut tips: Vec<String> = Vec::new();
    if let Some(q) = queries.last() {
        let words = q.q.split_whitespace().count();
        if words >= 2 {
            tips.push(
                "multi-word query: mode=auto already escalated to semantic and still found nothing — try fewer/exact keywords or mode=ast".into(),
            );
        } else if !q.q.trim().is_empty() {
            tips.push(
                "try mode=structural for an exact symbol name, mode=semantic for a concept, or detail=refs to resolve it".into(),
            );
        }
        if q.path_scope.iter().any(|g| g.starts_with('!')) {
            tips.push("an exclusion (`!`) in path_scope may be over-filtering".into());
        } else if !q.path_scope.is_empty() {
            tips.push("widen path_scope (it currently restricts the search)".into());
        }
        if q.lang.is_some() {
            tips.push("drop the lang filter".into());
        }
    }
    if tips.is_empty() {
        tips.push("widen path_scope, drop the lang filter, or use detail=locate to scan".into());
    }
    format!("0 matches. {}", tips.join("; "))
}
/// Hint for SCOPE_NO_MATCH (SCRY-127): the filters, not the pattern, excluded
/// every file. Name the culprit and how to widen it — and note that a plain
/// `path_scope` entry is now a lenient path-component match, so the likeliest
/// remaining cause is a typo or a `lang` mismatch.
fn scope_no_match_hint(q: &Query) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !q.path_scope.is_empty() {
        parts.push(format!("path_scope={:?}", q.path_scope));
    }
    if let Some(l) = &q.lang {
        parts.push(format!("lang={l}"));
    }
    format!(
        "scope matched 0 files ({}) — the FILTER excluded everything, the pattern was never searched. \
         Widen or drop path_scope/lang (a plain entry like `config.dart` matches by basename/subpath; \
         check for a typo or a lang mismatch), or re-run with no scope.",
        parts.join(", ")
    )
}
/// Bounded Levenshtein edit distance (SCRY-137): returns the distance if it is
/// `<= max`, else `None`. Early-exits when a whole DP row exceeds `max`, so a
/// far-apart pair costs O(max·len), not O(len²). Used only for typo recovery.
fn levenshtein_within(a: &str, b: &str, max: u32) -> Option<u32> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (la, lb) = (a.len(), b.len());
    if la.abs_diff(lb) > max as usize {
        return None;
    }
    let mut prev: Vec<u32> = (0..=lb as u32).collect();
    let mut cur = vec![0u32; lb + 1];
    for i in 1..=la {
        cur[0] = i as u32;
        let mut row_min = cur[0];
        for j in 1..=lb {
            let cost = u32::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None; // every alignment through this row already exceeds max
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[lb];
    (d <= max).then_some(d)
}
/// Does this query carry boolean operands (the all_of/any_of/none_of path)?
fn has_bool(q: &Query) -> bool {
    !q.all_of.is_empty() || !q.any_of.is_empty() || !q.none_of.is_empty()
}

/// Candidate files for a plain-identifier lexical query, honoring SUBSTRING
/// recall (SCRY-038). The inverted index is keyed by *whole* identifiers, so an
/// exact `postings.get(needle)` under-selects whenever the query is a proper
/// substring of a larger identifier — e.g. `token` ⊂ `build_token_index`, or
/// `confiden` ⊂ `low_confidence`. A regex/`memmem` match on a plain-identifier
/// query always lands *inside* a single identifier token (identifier chars are
/// contiguous), so unioning every key that contains the needle is exactly the
/// set of files that can match — full recall, still pruned (no byte read for a
/// non-candidate file). Keys are lowercased, so pass a lowercased needle.
/// SCRY-083: Levenshtein edit distance — used only on a failed read-by-path to
/// suggest the closest indexed filename (typo / wrong-dir recovery). O(m·n); run
/// per indexed file only on the error path, so the cost is off the hot path.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn postings_substring(postings: &HashMap<String, Vec<String>>, needle: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    // Defensive: an empty needle is a substring of every key — returning the
    // whole repo would be a silent footgun. Callers already gate on
    // `is_plain_ident` (len ≥ 3), but never trust that here.
    if needle.is_empty() {
        return out;
    }
    for (k, files) in postings {
        if k.contains(needle) {
            out.extend(files.iter().cloned());
        }
    }
    out
}
/// Cached semantic (subword-TF-IDF) index — per-symbol token bags + corpus
/// document-frequency. Building it is the cost the `mode=auto` escalation and
/// `mode=semantic` pay; caching it by revision (like the token index) makes
/// repeated low-confidence queries cheap (rebuilt only when a write bumps the
/// revision). Whole-repo; per-query `path_scope`/`lang` filter at score time.
struct SemanticIndex {
    docs: Vec<(String, vyer_incr::Symbol, std::collections::HashSet<String>)>,
    df: HashMap<String, usize>,
    /// SCRY-080: subword → doc indices, so scoring touches only docs that share a
    /// query subword (the only ones that can score > 0) instead of the whole
    /// corpus — semantic warm latency from O(corpus) to O(candidates).
    postings: HashMap<String, Vec<usize>>,
}

fn build_semantic_index(db: &Db) -> SemanticIndex {
    // SCRY-096 (reverted): a per-file cached-bags incremental build was MEASURED at
    // 50k to make the post-edit rebuild WORSE (133ms → 179ms), not better — the
    // bottleneck is the global df/postings/docs construction over ~500k tokens, not
    // the `subword_tokens` pass, so caching tokenization only adds hash-check + clone
    // overhead. The honest conclusion: this opt-in mode's post-edit rebuild is bound
    // by the tf-idf corpus structure; a true incremental would need stable doc-ids and
    // delta-maintained df/postings (not justified for an off-by-default mode). Kept the
    // simple from-scratch build; the violation is documented in §7 / the bench.
    let mut docs: Vec<(String, vyer_incr::Symbol, std::collections::HashSet<String>)> = Vec::new();
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut postings: HashMap<String, Vec<usize>> = HashMap::new();
    for f in db.files() {
        for s in &db.symbols(&f).symbols {
            let mut bag: std::collections::HashSet<String> = std::collections::HashSet::new();
            bag.extend(subword_tokens(&s.name));
            bag.extend(subword_tokens(&s.signature));
            let idx = docs.len();
            for t in &bag {
                *df.entry(t.clone()).or_insert(0) += 1;
                postings.entry(t.clone()).or_default().push(idx);
            }
            docs.push((f.clone(), s.clone(), bag));
        }
    }
    SemanticIndex { docs, df, postings }
}
/// Build the token→files inverted index over the warm core's current contents.
/// Remove every posting contributed by `file` for `toks` (dropping a token whose
/// file list becomes empty). The inverse of adding a file's tokens.
fn remove_file_tokens(postings: &mut HashMap<String, Vec<String>>, file: &str, toks: &[String]) {
    for t in toks {
        if let Some(v) = postings.get_mut(t) {
            v.retain(|x| x != file);
            if v.is_empty() {
                postings.remove(t);
            }
        }
    }
}

/// SCRY-079: bring the token index up to the warm core's current contents,
/// touching only files whose content hash changed since the last update (plus
/// removing files that are gone). Tokens are lowercased identifiers (len ≥ 3),
/// deduped per file by `identifiers`. The result is identical to a from-scratch
/// build — `update_token_index` on an empty index IS the full build — but a
/// single-file edit costs O(changed files), not O(repo). Unchanged files cost one
/// hash compare (no text read, no clone).
fn update_token_index(idx: &mut TokenIndex, db: &Db) {
    let current: HashSet<String> = db.files().into_iter().collect();
    // drop files that no longer exist
    let gone: Vec<String> = idx
        .file_tokens
        .keys()
        .filter(|f| !current.contains(f.as_str()))
        .cloned()
        .collect();
    for f in gone {
        if let Some((_, toks)) = idx.file_tokens.remove(&f) {
            remove_file_tokens(&mut idx.postings, &f, &toks);
        }
    }
    // (re)index new or changed files
    for f in &current {
        let h = match db.content_hash(f) {
            Some(h) => h,
            None => continue,
        };
        if matches!(idx.file_tokens.get(f), Some((old, _)) if *old == h) {
            continue; // unchanged
        }
        if let Some(old) = idx.file_tokens.get(f).map(|(_, t)| t.clone()) {
            remove_file_tokens(&mut idx.postings, f, &old);
        }
        if let Some(text) = db.text(f) {
            let toks: Vec<String> = identifiers(&text)
                .iter()
                .map(|t| t.to_ascii_lowercase())
                .collect();
            for t in &toks {
                idx.postings.entry(t.clone()).or_default().push(f.clone());
            }
            idx.file_tokens.insert(f.clone(), (h, toks));
        }
    }
}

/// Ensure the cached token index reflects the current revision, building it (from
/// empty) or updating it incrementally as needed (SCRY-079).
fn ensure_token_index(slot: &mut Option<(u64, TokenIndex)>, db: &Db) {
    let rev = db.revision();
    match slot.as_mut() {
        Some((r, idx)) if *r != rev => {
            update_token_index(idx, db);
            *r = rev;
        }
        Some(_) => {}
        None => {
            let mut idx = TokenIndex::default();
            update_token_index(&mut idx, db);
            *slot = Some((rev, idx));
        }
    }
}

/// Directory names we never index (build output / dependency / VCS dirs). Keeps
/// indexing fast and relevant even without a `.gitignore`.
fn is_skippable_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | "dist"
            | "build"
            | ".venv"
            | "venv"
            | "vendor"
            | ".next"
            | "__pycache__"
            | ".mypy_cache"
            | ".pytest_cache"
    )
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// Extensions for a `lang` filter. Accepts a single language (`ts`) or a
/// comma-separated list (`ts,js`) for polyglot repos — the union of each token's
/// extensions, de-duplicated, in order. Aliases (`rs`/`rust`, `py`/`python`, …)
/// are resolved per token by `lang_exts_one`.
fn lang_extensions(lang: &str) -> Vec<&'static str> {
    if !lang.contains(',') {
        return lang_exts_one(lang);
    }
    let mut out: Vec<&'static str> = Vec::new();
    for part in lang.split(',') {
        for e in lang_exts_one(part) {
            if !out.contains(&e) {
                out.push(e);
            }
        }
    }
    out
}
/// File extensions for ONE language token (with common aliases).
fn lang_exts_one(lang: &str) -> Vec<&'static str> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => vec![".rs"],
        "python" | "py" => vec![".py", ".pyi", ".pyw"], // SCRY-104: stubs + Windows GUI
        // SCRY-101: keep these in sync with detect_lang (SCRY-100 added .mts/.cts/.cjs);
        // `lang:ts` must match `.mts`/`.cts` and `lang:js` must match `.cjs`.
        "js" | "javascript" => vec![".js", ".jsx", ".mjs", ".cjs"],
        "ts" | "typescript" => vec![".ts", ".tsx", ".mts", ".cts"],
        "go" | "golang" => vec![".go"],
        "dart" => vec![".dart"],
        "java" => vec![".java"],
        "ruby" | "rb" => vec![".rb"],
        "swift" => vec![".swift"],
        "kotlin" | "kt" => vec![".kt", ".kts"],
        // SCRY-103: `.h` is ambiguous — it matches BOTH `lang:c` and `lang:cpp`.
        "c" => vec![".c", ".h"],
        "cpp" | "c++" => vec![".cpp", ".cc", ".cxx", ".hpp", ".h", ".hh", ".hxx"],
        "cs" | "csharp" => vec![".cs"],
        "php" => vec![".php"],
        // SCRY-124 (#4): common non-tree-sitter text formats are still worth
        // FILTERING by extension (search/tree/count), even without a parser.
        "yaml" | "yml" => vec![".yaml", ".yml"],
        "json" => vec![".json"],
        "toml" => vec![".toml"],
        "dockerfile" | "docker" => vec![".dockerfile", "Dockerfile"],
        "md" | "markdown" => vec![".md", ".markdown"],
        "html" => vec![".html", ".htm"],
        "css" => vec![".css"],
        "sh" | "bash" | "shell" => vec![".sh", ".bash"],
        "xml" => vec![".xml"],
        _ => vec![],
    }
}

/// SCRY-107: the `Lang` enums a `lang:` filter name (csv) refers to — lets the filter
/// match an EXTENSIONLESS shebang-detected file (SCRY-106) by its detected language,
/// not just by extension. Mirrors `lang_exts_one`.
fn lang_enums(lang: &str) -> Vec<vyer_incr::Lang> {
    lang.split(',').filter_map(lang_name_to_enum).collect()
}

fn lang_name_to_enum(name: &str) -> Option<vyer_incr::Lang> {
    use vyer_incr::Lang;
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" => Lang::Python,
        "js" | "javascript" => Lang::JavaScript,
        "ts" | "typescript" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "go" | "golang" => Lang::Go,
        "dart" => Lang::Dart,
        "java" => Lang::Java,
        "ruby" | "rb" => Lang::Ruby,
        "swift" => Lang::Swift,
        "kotlin" | "kt" => Lang::Kotlin,
        "c" => Lang::C,
        "cpp" | "c++" => Lang::Cpp,
        "cs" | "csharp" => Lang::CSharp,
        "php" => Lang::Php,
        _ => return None,
    })
}

/// SCRY-107: true if the path's basename carries no extension (e.g. `scripts/deploy`),
/// so the lang filter knows to consult the shebang-detected language for it.
fn is_extensionless(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    !base.contains('.')
}

/// The innermost symbol whose span contains `line`, as (name, start, end).
/// Falls back to a single-line span when no symbol encloses the hit.
fn enclosing(syms: &vyer_incr::SymbolTable, line: u32) -> (Option<String>, u32, u32) {
    let mut best: Option<&vyer_incr::Symbol> = None;
    for s in &syms.symbols {
        if s.start <= line && line <= s.end {
            match best {
                Some(b) if (b.end - b.start) <= (s.end - s.start) => {}
                _ => best = Some(s),
            }
        }
    }
    match best {
        Some(s) => (Some(s.name.clone()), s.start, s.end),
        None => (None, line, line),
    }
}

/// SCRY-116: extract (path, 1-based line) references from a compiler / test / stack-trace
/// blob across the common toolchain formats — deterministic, dependency-free:
///   `src/a.rs:42:10` · `lib/a.dart:42:5` · `./a.go:42` · `a.ts(42,10)` ·
///   `File "a.py", line 42` · `at f (src/a.js:42:10)`
/// Deduped, in first-seen order (the first reference is usually the root cause).
/// SCRY-141: auto-derive a `code_run` task allowlist from the repo's manifests, so
/// `--allow-run` alone gives the agent a working build/test/lint loop (zero config).
/// Only SAFE, non-mutating tasks (build/check/test/lint/analyze/vet) — never `fmt`
/// (mutates) or `run`/serve (long-running). Operator-gated regardless (it's only
/// consulted when --allow-run is set); the agent still selects a task by NAME, so
/// Rule §3 holds. First stack wins on a name collision; explicit --run overrides.
pub fn derive_run_tasks(root: &Path) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut tasks: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let exists = |n: &str| root.join(n).exists();
    let argv = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    let mut add = |name: &str, cmd: &str| {
        tasks.entry(name.to_string()).or_insert_with(|| argv(cmd));
    };
    if exists("Cargo.toml") {
        add("check", "cargo check");
        add("test", "cargo test");
        add("lint", "cargo clippy");
        add("build", "cargo build");
    }
    if exists("go.mod") {
        add("build", "go build ./...");
        add("test", "go test ./...");
        add("lint", "go vet ./...");
    }
    if exists("pubspec.yaml") {
        let dart = std::fs::read_to_string(root.join("pubspec.yaml")).unwrap_or_default();
        let t = if dart.contains("sdk: flutter") || dart.contains("\nflutter:") {
            "flutter"
        } else {
            "dart"
        };
        add("test", &format!("{t} test"));
        add("lint", "dart analyze");
    }
    if exists("pyproject.toml") || exists("setup.py") || exists("requirements.txt") {
        add("test", "pytest");
    }
    if exists("package.json") {
        // Only wire scripts that actually exist, so a task never fails as "missing
        // script". `npm test`/`npm run X` are stable entry points.
        if let Ok(pkg) = std::fs::read_to_string(root.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
                let scripts = v.get("scripts").and_then(|s| s.as_object());
                let has = |k: &str| scripts.is_some_and(|m| m.contains_key(k));
                if has("test") {
                    add("test", "npm test");
                }
                if has("build") {
                    add("build", "npm run build");
                }
                if has("lint") {
                    add("lint", "npm run lint");
                }
                if has("typecheck") {
                    add("check", "npm run typecheck");
                }
            }
        }
    }
    tasks
}

/// A parsed diagnostic (SCRY-133): WHERE (path+line) plus best-effort WHAT
/// (severity + message). severity/message are absent for formats that don't carry
/// them on the location line (e.g. a Python traceback frame). This is the
/// structured back-half of the run→error→fix loop — the agent never hand-parses a
/// stack trace.
#[derive(Debug, Clone)]
struct Diag {
    path: String,
    line: u32,
    severity: Option<String>,
    message: Option<String>,
}

fn parse_diagnostics(blob: &str) -> Vec<Diag> {
    fn looks_like_path(p: &str) -> bool {
        !p.is_empty() && (p.contains('/') || p.contains('\\') || p.contains('.'))
    }
    fn alldigit(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    // Best-effort severity + message from ONE line, format-agnostic: find the
    // severity keyword, then the message is the text after the next `: ` (handles
    // `error[E0308]: msg`, `error: msg`, `error TS1: msg`, `Error: msg`).
    fn sev_msg(raw: &str) -> (Option<String>, Option<String>) {
        let lower = raw.to_ascii_lowercase();
        let (sev, kw) = if let Some(p) = lower.find("error") {
            (Some("error"), Some(p))
        } else if let Some(p) = lower.find("warning") {
            (Some("warning"), Some(p))
        } else if let Some(p) = lower.find("warn") {
            (Some("warning"), Some(p))
        } else if let Some(p) = lower.find("note") {
            (Some("note"), Some(p))
        } else {
            (None, None)
        };
        let msg = kw
            .and_then(|p| {
                raw[p..]
                    .find(": ")
                    .map(|i| raw[p + i + 2..].trim().to_string())
            })
            .filter(|m| !m.is_empty());
        (sev.map(|s| s.to_string()), msg)
    }
    fn push(
        out: &mut Vec<Diag>,
        path: &str,
        line: u32,
        sev: &Option<String>,
        msg: &Option<String>,
    ) {
        let path = path.trim_matches(|c: char| ",;: \t".contains(c));
        if line == 0 || !looks_like_path(path) {
            return;
        }
        if !out.iter().any(|d| d.path == path && d.line == line) {
            out.push(Diag {
                path: path.to_string(),
                line,
                severity: sev.clone(),
                message: msg.clone(),
            });
        }
    }
    let mut out: Vec<Diag> = Vec::new();
    // Rust prints the severity+message on the `error[..]:` line and the LOCATION on
    // the following `--> file:line` line, so carry the most recent over to a `-->`.
    let mut last_sev: Option<String> = None;
    let mut last_msg: Option<String> = None;
    for raw in blob.lines() {
        let (mut sev, mut msg) = sev_msg(raw);
        if sev.is_some() {
            last_sev = sev.clone();
            last_msg = msg.clone();
        } else if raw.contains("-->") {
            sev = last_sev.clone();
            msg = last_msg.clone();
        }
        // Python: File "path", line N
        if let Some(fi) = raw.find("File \"") {
            let rest = &raw[fi + 6..];
            if let Some(qend) = rest.find('"') {
                let path = &rest[..qend];
                if let Some(li) = rest[qend..].find("line ") {
                    let after = &rest[qend + li + 5..];
                    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(n) = num.parse() {
                        push(&mut out, path, n, &sev, &msg);
                    }
                }
            }
        }
        // path:line[:col] — tokenized on whitespace + brackets/quotes (catches `(a.js:42:10)`).
        // Trim a trailing `:`/`,` so the GCC/Dart `path:line:col:` form (trailing colon) and
        // `path:line,` still right-split cleanly.
        for tok in raw.split(|c: char| c.is_whitespace() || "()[]{}'\"`<>".contains(c)) {
            let tok = tok.trim_end_matches([':', ',']);
            let parts: Vec<&str> = tok.rsplitn(3, ':').collect();
            if parts.len() == 3 && alldigit(parts[0]) && alldigit(parts[1]) {
                push(
                    &mut out,
                    parts[2],
                    parts[1].parse().unwrap_or(0),
                    &sev,
                    &msg,
                );
            } else if parts.len() == 2 && alldigit(parts[0]) {
                push(
                    &mut out,
                    parts[1],
                    parts[0].parse().unwrap_or(0),
                    &sev,
                    &msg,
                );
            }
        }
        // path(line[,col]) — tsc / C# / msbuild.
        let mut search = 0;
        while let Some(rel) = raw[search..].find('(') {
            let open = search + rel;
            let after = &raw[open + 1..];
            let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !num.is_empty() && matches!(after.as_bytes().get(num.len()), Some(b',') | Some(b')'))
            {
                let path = raw[..open]
                    .rsplit(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                push(&mut out, path, num.parse().unwrap_or(0), &sev, &msg);
            }
            search = open + 1;
        }
    }
    out
}

/// SCRY-118: relative path from `from_file`'s directory to `to_file` (both repo-relative),
/// e.g. lib/screens/home.dart → lib/models/user.dart  ⇒  ../models/user.dart. For JS/Dart imports.
fn relpath(from_file: &str, to_file: &str) -> String {
    let from: Vec<&str> = from_file.split('/').collect();
    let to: Vec<&str> = to_file.split('/').collect();
    let from_dir = &from[..from.len().saturating_sub(1)];
    let mut i = 0;
    while i < from_dir.len() && i + 1 < to.len() && from_dir[i] == to[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..(from_dir.len() - i) {
        parts.push("..".to_string());
    }
    for c in &to[i..] {
        parts.push((*c).to_string());
    }
    let rel = parts.join("/");
    if rel.starts_with("..") {
        rel
    } else {
        format!("./{rel}")
    }
}

/// SCRY-118: construct the import statement bringing `sym` (defined in `def`) into `target`, in
/// `target`'s language. Relative-path languages (TS/JS/Dart) are exact — vyer verified the symbol
/// is defined in `def` and the file exists; module-path languages (Python/Rust) are best-effort
/// (the agent verifies); Go is package-level → a note. Always actionable.
fn build_import(sym: &str, target: &str, def: &str) -> String {
    let strip_ext = |p: &str| {
        p.rsplit_once('.')
            .map(|(a, _)| a.to_string())
            .unwrap_or_else(|| p.to_string())
    };
    match target.rsplit('.').next().unwrap_or("") {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            format!(
                "import {{ {sym} }} from '{}';",
                strip_ext(&relpath(target, def))
            )
        }
        "dart" => format!("import '{}';", relpath(target, def)),
        "py" => format!(
            "from {} import {sym}    # best-effort module path — verify",
            strip_ext(def).replace('/', ".")
        ),
        "rs" => {
            let m = def.rsplit_once("src/").map(|(_, b)| b).unwrap_or(def);
            format!(
                "use crate::{}::{sym};    // best-effort module path — verify",
                strip_ext(m).replace('/', "::")
            )
        }
        "go" => format!(
            "// `{sym}` is in {def}; Go imports are package-level — import that package path"
        ),
        _ => format!("// `{sym}` is defined in {def}; add an import in {target}'s style"),
    }
}

fn make_id(path: &str, symbol: Option<&str>, start: u32, end: u32, text: &str) -> String {
    let loc = Locator {
        path: path.to_string(),
        symbol: symbol.map(|s| s.to_string()),
        start,
        end,
        // SCRY-015: hash the SYMBOL's own line span, not the whole file, so the
        // locator's staleness hash changes iff *this* symbol drifts — editing one
        // function no longer invalidates every other symbol's locator in the file.
        hash: Some(short_hash(&symbol_slice(text, start, end))),
    };
    loc.format()
}

/// The text of 1-based inclusive lines `[start, end]` — the per-symbol slice the
/// locator hash is computed over (SCRY-015). Falls back to the whole text for a
/// degenerate range so an ill-formed span still produces a stable hash.
fn symbol_slice(text: &str, start: u32, end: u32) -> String {
    if start == 0 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let s = (start as usize).saturating_sub(1);
    let e = (end as usize).min(lines.len());
    if s >= e {
        return text.to_string();
    }
    lines[s..e].join("\n")
}

/// Heuristic: is this an unambiguously MACHINE-GENERATED file? Such files are
/// derived artifacts (codegen / protobuf / minified) an agent rarely needs to
/// *understand*, so the repo-map demotes them below hand-written code. Only
/// strong, near-universal suffix conventions — never a guess on plain sources.
fn is_generated(path: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".g.dart",
        ".freezed.dart",
        ".pb.go",
        ".pb.dart",
        "_pb2.py",
        "_pb2_grpc.py",
        ".min.js",
        ".min.css",
        ".gen.go",
        ".generated.go",
        ".designer.cs",
    ];
    let p = path.to_ascii_lowercase();
    SUFFIXES.iter().any(|s| p.ends_with(s))
        || p.contains(".generated.")
        // SCRY-122: files under a `generated/` (or `__generated__/`) directory are
        // codegen too — tag + demote them like the suffix-based artifacts.
        || p.split('/')
            .any(|seg| seg == "generated" || seg == "__generated__")
}

/// SCRY-123: cap a unified-diff body so one large change can't dominate the
/// output budget. Keeps the first `max_lines` lines + a one-line remainder note.
fn cap_diff(diff: &str, max_lines: usize) -> String {
    let total = diff.lines().count();
    if total <= max_lines {
        return diff.to_string();
    }
    let head: String = diff.lines().take(max_lines).collect::<Vec<_>>().join("\n");
    format!(
        "{head}\n… +{} more diff lines (read the file or narrow with q=path)",
        total - max_lines
    )
}

/// Package/build manifests that mark a directory as a project root (SCRY-126).
const PKG_MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "build.gradle",
    "build.gradle.kts",
    "pom.xml",
    "pubspec.yaml",
    "Gemfile",
];

/// SCRY-126: the nearest ancestor directory of `file` that holds a package
/// manifest (the package boundary), searched among the indexed `files`. Returns
/// the directory prefix (e.g. `packages/cli`), `""` when only the repo root has a
/// manifest, or `None` if none is found. Used to confine an ambiguous rename and
/// to scope a qualified-locator refs/impact to one package.
fn package_root(files: &[String], file: &str) -> Option<String> {
    let mut dir = file;
    loop {
        let parent = match dir.rfind('/') {
            Some(i) => &dir[..i],
            None => "",
        };
        let has_manifest = PKG_MANIFESTS.iter().any(|m| {
            let cand = if parent.is_empty() {
                m.to_string()
            } else {
                format!("{parent}/{m}")
            };
            files.iter().any(|f| f == &cand)
        });
        if has_manifest {
            return Some(parent.to_string());
        }
        if parent.is_empty() {
            return None;
        }
        dir = parent;
    }
}

/// Short content hash for staleness detection in the locator. (blake3 in the
/// design; a stable std hash here keeps the core zero-dependency — the locator
/// format is identical and tree-sitter/blake3 swap in without an interface change.)
fn short_hash(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn rank_by_count(counts: HashMap<String, usize>) -> Vec<String> {
    let mut v: Vec<(usize, String)> = counts.into_iter().map(|(id, c)| (c, id)).collect();
    // most hits first; id asc for deterministic tie-break.
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.into_iter().map(|(_, id)| id).collect()
}

/// Parse a 1-based, inclusive line-range spec against a file of `total` lines.
/// Grammar (deterministic, no regex): `40-80` (range), `40` (single), `40-`
/// (to EOF), `-80` (first 80 = head), `~20` (last 20 = tail). Returns a clamped
/// `(start, end)` with `1 <= start <= end <= total`. `Err(hint)` for a
/// non-numeric or empty spec; the caller turns it into an actionable span.
fn parse_line_range(spec: &str, total: u32) -> Result<(u32, u32), String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("expected e.g. `40-80`, `40`, `40-`, `-80` (head), or `~20` (tail)".into());
    }
    let total = total.max(1);
    let num = |x: &str| -> Result<u32, String> {
        x.trim()
            .parse::<u32>()
            .map_err(|_| format!("{x:?} is not a line number"))
    };
    let (mut start, mut end) = if let Some(rest) = s.strip_prefix('~') {
        // tail: the last N lines
        let n = num(rest)?.max(1);
        (total.saturating_sub(n - 1).max(1), total)
    } else if let Some(rest) = s.strip_prefix('-') {
        // head: the first N lines
        (1, num(rest)?.max(1))
    } else if let Some(rest) = s.strip_suffix('-') {
        // from N to EOF
        (num(rest)?.max(1), total)
    } else if let Some((a, b)) = s.split_once('-') {
        (num(a)?.max(1), num(b)?)
    } else {
        let n = num(s)?.max(1);
        (n, n)
    };
    // SCRY-084: reject a reversed range on the PARSED values, before clamping —
    // else `80-40` (both past EOF) clamps to `5-5` and slips past this check,
    // inconsistently with `4-2` (which errors). Head/tail/single are start≤end by
    // construction, so this only catches an explicitly-reversed `a-b`.
    if start > end {
        return Err(format!(
            "start {start} is past end {end} (file has {total} lines)"
        ));
    }
    start = start.clamp(1, total);
    end = end.min(total);
    Ok((start, end))
}

fn slice_lines(lines: &[&str], start: u32, end: u32, cap: usize) -> String {
    let s = (start as usize).saturating_sub(1);
    let e = (end as usize).min(lines.len());
    let take = (e.saturating_sub(s)).min(cap);
    lines[s..s + take].join("\n")
}

/// Prefix body lines with absolute line numbers (locate already embeds them).
fn number_lines(body: &str, start: u32, detail: &str) -> String {
    if detail == "locate" {
        return body.to_string();
    }
    let mut out = String::new();
    for (i, line) in body.lines().enumerate() {
        out.push_str(&format!("{}: {}\n", start as usize + i, line));
    }
    out
}

/// Whether `path` is in scope given `scopes` globs (SCRY-040). A `!`-prefixed
/// glob is an EXCLUSION: the path is rejected outright if it matches any. Among
/// the positive globs the path must match at least one — unless there are no
/// positive globs (empty, or exclusions only), in which case everything not
/// excluded is in scope. Lets an agent write `["src/**", "!**/tests/**"]`.
pub fn path_in_scope(path: &str, scopes: &[String]) -> bool {
    let mut has_pos = false;
    let mut pos_match = false;
    for g in scopes {
        if let Some(neg) = g.strip_prefix('!') {
            if scope_entry_match(neg, path) {
                return false;
            }
        } else {
            has_pos = true;
            if scope_entry_match(g, path) {
                pos_match = true;
            }
        }
    }
    !has_pos || pos_match
}

/// Match one scope entry against a path. A glob (`*`/`?`) keeps EXACT glob
/// semantics; a wildcard-free entry is treated leniently as a path *component*
/// match (SCRY-127) — the grep-equivalence floor. The old strict behavior made
/// `path_scope:["config.dart"]` (or even `["lib/game/config.dart"]` for a nested
/// match) silently match NOTHING, so the scope ate the whole search and the
/// engine then misreported it as PATTERN_NO_MATCH. An agent that names a file or
/// directory means "scope me to it", so a plain entry `P` matches when the path
/// equals `P`, ends with `/P` (a file or trailing subpath), starts with `P/` (a
/// top-level dir), or contains `/P/` (an interior dir). Explicit globs are
/// untouched, so `*.rs` / `src/**` keep their precise meaning.
fn scope_entry_match(entry: &str, path: &str) -> bool {
    if entry.bytes().any(|b| matches!(b, b'*' | b'?')) {
        return glob_match(entry, path);
    }
    let e = entry.trim_matches('/');
    if e.is_empty() {
        return false;
    }
    path == e
        || path.ends_with(&format!("/{e}"))
        || path.starts_with(&format!("{e}/"))
        || path.contains(&format!("/{e}/"))
}
/// Minimal glob: supports `*` (within a path segment), `**` (across segments),
/// and `?` (any non-`/` char). Enough for `src/auth/**` / `**/*.rs` scoping
/// without pulling in a glob dependency. Simple recursive matcher.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    glob_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_inner(pat: &[u8], txt: &[u8]) -> bool {
    if pat.is_empty() {
        return txt.is_empty();
    }
    match pat[0] {
        b'*' => {
            if pat.len() >= 2 && pat[1] == b'*' {
                // `**` (optionally followed by `/`) matches across segments.
                let rest = if pat.len() >= 3 && pat[2] == b'/' {
                    &pat[3..]
                } else {
                    &pat[2..]
                };
                // try consuming 0..=all of txt (including '/')
                if glob_inner(rest, txt) {
                    return true;
                }
                for i in 0..txt.len() {
                    if glob_inner(rest, &txt[i + 1..]) {
                        return true;
                    }
                }
                false
            } else {
                // single `*` matches within a segment (no '/').
                if glob_inner(&pat[1..], txt) {
                    return true;
                }
                for i in 0..txt.len() {
                    if txt[i] == b'/' {
                        break;
                    }
                    if glob_inner(&pat[1..], &txt[i + 1..]) {
                        return true;
                    }
                }
                false
            }
        }
        b'?' => !txt.is_empty() && txt[0] != b'/' && glob_inner(&pat[1..], &txt[1..]),
        c => !txt.is_empty() && txt[0] == c && glob_inner(&pat[1..], &txt[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_segments_and_globstar() {
        assert!(glob_match("src/auth/**", "src/auth/token.rs"));
        assert!(glob_match("src/auth/**", "src/auth/sub/deep.rs"));
        assert!(!glob_match("src/auth/**", "src/other/x.rs"));
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "a/main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn line_range_grammar() {
        // total = 100 lines
        assert_eq!(parse_line_range("40-80", 100), Ok((40, 80)));
        assert_eq!(parse_line_range("40", 100), Ok((40, 40)));
        assert_eq!(parse_line_range("40-", 100), Ok((40, 100))); // to EOF
        assert_eq!(parse_line_range("-80", 100), Ok((1, 80))); // head
        assert_eq!(parse_line_range("~20", 100), Ok((81, 100))); // tail
        assert_eq!(parse_line_range("~1", 100), Ok((100, 100))); // tail of 1
                                                                 // clamping: past EOF is clamped, not an error
        assert_eq!(parse_line_range("90-999", 100), Ok((90, 100)));
        assert_eq!(parse_line_range("~999", 100), Ok((1, 100))); // tail bigger than file
                                                                 // a one-line file
        assert_eq!(parse_line_range("~5", 1), Ok((1, 1)));
        // errors: non-numeric / empty / inverted
        assert!(parse_line_range("abc", 100).is_err());
        assert!(parse_line_range("", 100).is_err());
        assert!(parse_line_range("80-40", 100).is_err());
        // SCRY-084: a reversed range whose ends are BOTH past EOF must still error
        // (it used to clamp to `last-last` and slip past the start>end check).
        assert!(parse_line_range("80-40", 5).is_err());
        assert!(parse_line_range("999-998", 5).is_err());
        // a non-reversed range past EOF still clamps (not an error).
        assert_eq!(parse_line_range("3-999", 5), Ok((3, 5)));
    }

    #[test]
    fn line_range_read_slices_only_the_range() {
        let cfg = EngineConfig::new(std::env::temp_dir());
        let engine = Engine {
            config: cfg,
            db: Mutex::new(Db::new()),
            seen: Mutex::new(HashSet::new()),
            audit: Mutex::new(Vec::new()),
            token_index: Mutex::new(None),
            semantic_index: Mutex::new(None),
            history: Mutex::new(Vec::new()),
        };
        {
            let mut db = engine.db.lock().unwrap();
            let body: String = (1..=50).map(|i| format!("line {i}\n")).collect();
            db.set_text("f.txt", &body);
        }
        let db = engine.db.lock().unwrap();
        let q = Query {
            path: Some("f.txt".into()),
            lines: Some("10-12".into()),
            ..Query {
                q: String::new(),
                path: None,
                mode: "auto".into(),
                detail: "snippet".into(),
                path_scope: vec![],
                lang: None,
                lines: None,
                all_of: vec![],
                any_of: vec![],
                none_of: vec![],
                k: 8,
            }
        };
        let spans = engine.read_path_spans(&db, &q, "f.txt", 8000);
        assert_eq!(spans.len(), 1);
        let t = &spans[0].text;
        assert!(t.contains("10: line 10"), "got: {t}");
        assert!(t.contains("12: line 12"), "got: {t}");
        assert!(!t.contains("line 9"), "leaked before range: {t}");
        assert!(!t.contains("line 13"), "leaked after range: {t}");
        // tail
        let qt = Query {
            lines: Some("~3".into()),
            ..q.clone()
        };
        let st = engine.read_path_spans(&db, &qt, "f.txt", 8000);
        assert!(st[0].text.contains("48: line 48"));
        assert!(st[0].text.contains("50: line 50"));
        assert!(!st[0].text.contains("47: line 47"));
    }

    fn engine_with(files: &[(&str, &str)]) -> Engine {
        let engine = Engine {
            config: EngineConfig::new(std::env::temp_dir()),
            db: Mutex::new(Db::new()),
            seen: Mutex::new(HashSet::new()),
            audit: Mutex::new(Vec::new()),
            token_index: Mutex::new(None),
            semantic_index: Mutex::new(None),
            history: Mutex::new(Vec::new()),
        };
        {
            let mut db = engine.db.lock().unwrap();
            for (p, t) in files {
                db.set_text(p, t);
            }
        }
        engine
    }

    #[test]
    fn status_surfaces_skipped_large_files() {
        // SCRY-092: a file over max_file_bytes is skipped at index time; status must
        // SURFACE it (honest degradation, §8) rather than leave it silently absent.
        let dir = std::env::temp_dir().join("vyer_skip_large_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big.rs"), "fn big() {}\n// padding ".repeat(20)).unwrap();
        std::fs::write(dir.join("small.rs"), "fn small() {}\n").unwrap();
        let mut cfg = EngineConfig::new(dir.clone());
        cfg.max_file_bytes = 40; // tiny: big.rs (>40B) skipped, small.rs (<40B) indexed
        let engine = Engine::new(cfg).unwrap();
        let status = engine.status();
        assert!(
            status.contains("skipped_large(>40B)=1"),
            "skip not surfaced: {status}"
        );
        assert!(
            status.contains("big.rs"),
            "skipped file name absent: {status}"
        );
        assert!(
            status.contains("indexed_files=1"),
            "only small.rs should index: {status}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_surfaces_sanitize_pathological_filenames() {
        // SCRY-095: file-derived paths in resource/data outputs (repo-map, status,
        // count, diff) must neutralize envelope markers — a pathological FILENAME
        // must not inject a fake span boundary (companion to 088/089 for non-span
        // output paths). repo-map is the representative file-derived path display.
        let dir = std::env::temp_dir().join("vyer_path_inject_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ev\u{27E6}s\u{27E7}.rs"),
            "fn a() { b(); }\nfn b() {}\n",
        )
        .unwrap();
        let engine = Engine::new(EngineConfig::new(dir.clone())).unwrap();
        let map = engine.repo_map(8000);
        // the FILENAME's markers must be neutralized; the envelope HEADER
        // (`⟦vyer/repo-map v1⟧`) legitimately uses `⟦⟧`, so check the path itself.
        assert!(
            !map.contains("ev\u{27E6}s\u{27E7}.rs"),
            "repo-map leaked the raw pathological filename: {map}"
        );
        assert!(
            map.contains("ev[s].rs"),
            "path not surfaced (sanitized): {map}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_history_is_bounded() {
        // SCRY-071: a long-lived daemon must not accumulate undo batches forever.
        let engine = engine_with(&[("a.rs", "fn a() {}\n")]);
        for i in 0..(MAX_UNDO_BATCHES + 50) {
            engine.record_history(vec![(format!("f{i}.rs"), Some("x".to_string()))]);
        }
        let hist = engine.history.lock().unwrap();
        assert_eq!(hist.len(), MAX_UNDO_BATCHES, "history must be capped");
        // the OLDEST batch dropped; the MOST RECENT is retained.
        let has = |name: &str| hist.iter().flatten().any(|(p, _)| p == name);
        assert!(
            has(&format!("f{}.rs", MAX_UNDO_BATCHES + 49)),
            "most recent batch must survive"
        );
        assert!(!has("f0.rs"), "oldest batch must have been evicted");
    }

    #[test]
    fn audit_log_is_bounded() {
        // SCRY-072: every call records an audit entry; a long-lived daemon must not
        // grow it without bound (the --audit file keeps the full record).
        let engine = engine_with(&[("a.rs", "fn a() {}\n")]);
        for i in 0..(MAX_AUDIT_ENTRIES + 50) {
            engine.record("code", format!("q{i}"));
        }
        assert_eq!(
            engine.audit.lock().unwrap().len(),
            MAX_AUDIT_ENTRIES,
            "in-memory audit must be capped"
        );
    }

    #[test]
    fn audit_entries_stay_single_line() {
        // SCRY-094: a summary carrying control chars (from a query/locator) must not
        // break the one-line tab-delimited audit format — no injected fake entry,
        // no broken columns (audit integrity, §9).
        let engine = engine_with(&[("a.rs", "fn a() {}\n")]);
        engine.record(
            "code",
            "q=evil\nFAKE\t9999\tcode_apply\tinjected".to_string(),
        );
        let audit = engine.audit.lock().unwrap();
        let last = audit.last().unwrap();
        assert!(
            !last.summary.contains('\n') && !last.summary.contains('\r'),
            "newline leaked into audit summary: {:?}",
            last.summary
        );
        assert!(
            !last.summary.contains('\t'),
            "tab leaked into audit summary (breaks TSV columns): {:?}",
            last.summary
        );
        assert_eq!(last.summary, "q=evil FAKE 9999 code_apply injected");
    }

    #[test]
    fn lang_filter_helpers_for_shebang_scripts() {
        // SCRY-107: extensionless detection (by BASENAME, ignoring dir dots) lets the
        // `lang:` filter match a shebang-detected script via name→Lang, not just by ext.
        assert!(is_extensionless("scripts/deploy"));
        assert!(is_extensionless("dir.v2/runner")); // basename `runner` has no extension
        assert!(!is_extensionless("a.py"));
        assert!(!is_extensionless("scripts/build.sh"));
        assert_eq!(lang_name_to_enum("python"), Some(vyer_incr::Lang::Python));
        assert_eq!(lang_name_to_enum("js"), Some(vyer_incr::Lang::JavaScript));
        assert_eq!(lang_name_to_enum("ruby"), Some(vyer_incr::Lang::Ruby));
        assert_eq!(lang_name_to_enum("nonsense"), None);
    }

    #[test]
    fn token_index_incremental_matches_full_rebuild() {
        // SCRY-079: after a sequence of edits the incrementally-maintained index
        // must EQUAL a from-scratch build — no stale or missing postings (freshness).
        let mut db = Db::new();
        db.set_text("a.rs", "fn alpha_one() {}\n");
        db.set_text("b.rs", "fn beta_two() {}\n");
        let mut idx = TokenIndex::default();
        update_token_index(&mut idx, &db);
        // edit a, add c, remove b
        db.set_text("a.rs", "fn alpha_changed() {}\n");
        db.set_text("c.rs", "fn gamma_three() {}\n");
        db.remove_text("b.rs");
        update_token_index(&mut idx, &db);
        // a fresh from-empty build of the same db must match exactly.
        let mut fresh = TokenIndex::default();
        update_token_index(&mut fresh, &db);
        let norm = |m: &HashMap<String, Vec<String>>| {
            let mut v: Vec<(String, Vec<String>)> = m
                .iter()
                .map(|(k, vv)| {
                    let mut s = vv.clone();
                    s.sort();
                    (k.clone(), s)
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            norm(&idx.postings),
            norm(&fresh.postings),
            "incremental index diverged from a full rebuild"
        );
        // stale tokens from the edited/removed files are gone; new ones present.
        assert!(
            !idx.postings.contains_key("alpha_one"),
            "stale token from an edited file"
        );
        assert!(idx.postings.contains_key("alpha_changed"));
        assert!(
            !idx.postings.contains_key("beta_two"),
            "stale token from a removed file"
        );
        assert!(idx.postings.contains_key("gamma_three"));
    }

    #[test]
    fn seen_paging_set_is_bounded() {
        // SCRY-073: a long session of exclude_seen calls must not grow `seen` forever.
        let engine = engine_with(&[("a.rs", "fn alpha() {}\n")]);
        {
            let mut seen = engine.seen.lock().unwrap();
            for i in 0..(MAX_SEEN_SPANS + 10) {
                seen.insert(format!("stale{i}"));
            }
        }
        let req = CodeRequest {
            queries: vec![Query {
                q: "alpha".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                k: 4,
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: true,
        };
        engine.code(&req);
        assert!(
            engine.seen.lock().unwrap().len() <= MAX_SEEN_SPANS,
            "the exclude_seen paging set must be bounded after a query"
        );
    }

    #[test]
    fn read_typo_path_suggests_the_closest_file() {
        // SCRY-083: a typo'd / wrong-dir read suggests the closest indexed file so
        // the agent recovers without a `tree` round-trip; unrelated names don't get
        // a bogus suggestion.
        let engine = engine_with(&[
            ("src/token.rs", "fn a() {}\n"),
            ("src/engine.rs", "fn b() {}\n"),
        ]);
        let db = engine.db.lock().unwrap();
        let q = base_query();
        let typo = engine.read_path_spans(&db, &q, "src/tokn.rs", 8000);
        assert!(
            typo[0].text.contains("did you mean") && typo[0].text.contains("src/token.rs"),
            "a typo should suggest the closest file: {}",
            typo[0].text
        );
        let wrongdir = engine.read_path_spans(&db, &q, "lib/token.rs", 8000);
        assert!(
            wrongdir[0].text.contains("src/token.rs"),
            "a right-name/wrong-dir should suggest the real path: {}",
            wrongdir[0].text
        );
        let unrelated = engine.read_path_spans(&db, &q, "zzzzzz.rs", 8000);
        assert!(
            unrelated[0].text.contains("check the spelling"),
            "an unrelated name must NOT get a bogus suggestion: {}",
            unrelated[0].text
        );
    }

    #[test]
    fn graph_mode_with_context_detail_routes_to_context_not_refs() {
        // SCRY-085: `mode=graph` defaults to `refs`, but `detail=context`/`impact`
        // must still DRIVE the operation (the documented [calls]/[called by]
        // grouping) — not be silently downgraded to refs by the mode.
        let engine = engine_with(&[(
            "m.rs",
            "fn helper_a() {}\nfn orchestrate() {\n    helper_a();\n}\nfn main() { orchestrate(); }\n",
        )]);
        let ctx = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "orchestrate".into(),
                mode: "graph".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            ctx.contains("[calls]") && ctx.contains("helper_a"),
            "graph+context must give the context grouping, not refs: {ctx}"
        );
        // a non-graph detail under mode=graph still defaults to refs.
        let refs = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "orchestrate".into(),
                mode: "graph".into(),
                detail: "snippet".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            refs.contains("references to"),
            "bare graph mode still defaults to refs: {refs}"
        );
    }

    #[test]
    fn structural_search_is_smart_case() {
        // SCRY-086: structural matching is smart-case like lexical — a lowercase
        // query is case-insensitive; an uppercase letter forces exact case.
        let engine = engine_with(&[("m.rs", "fn Engine_upper() {}\nfn engine_lower() {}\n")]);
        let req = |q: &str| CodeRequest {
            queries: vec![Query {
                q: q.into(),
                mode: "structural".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let upper = engine.code(&req("Engine"));
        assert!(
            upper.contains("Engine_upper") && !upper.contains("engine_lower"),
            "an uppercase letter must force exact case: {upper}"
        );
        let lower = engine.code(&req("engine"));
        assert!(
            lower.contains("Engine_upper") && lower.contains("engine_lower"),
            "an all-lowercase query must be case-insensitive: {lower}"
        );
    }

    #[test]
    fn replace_word_skips_strings_and_comments() {
        // SCRY-074: a rename touches CODE references but never occurrences inside
        // strings (data) or comments (prose) — consistent with refs/context.
        let rust = "fn foo() {}\n// foo here\nlet s = \"foo\";\nfoo()\n";
        let (out, n) = replace_word(rust, "foo", "bar", vyer_incr::Lang::Rust);
        assert_eq!(n, 2, "only the def + call are renamed");
        assert!(out.contains("fn bar()") && out.contains("bar()"));
        assert!(out.contains("// foo here"), "comment must be untouched");
        assert!(out.contains("\"foo\""), "string must be untouched");

        // Python: `#` comments and `'` strings.
        let py = "def foo():\n    # foo c\n    x = 'foo'\n    foo()\n";
        let (out2, n2) = replace_word(py, "foo", "bar", vyer_incr::Lang::Python);
        assert_eq!(n2, 2);
        assert!(out2.contains("# foo c"), "python comment must be untouched");
        assert!(out2.contains("'foo'"), "python string must be untouched");

        // SCRY-110: backtick strings — a JS template literal / Go raw string must NOT
        // be renamed (silent data corruption otherwise).
        let js = "function foo() {}\nfoo();\nconst m = `call foo now`;\n";
        let (out3, n3) = replace_word(js, "foo", "bar", vyer_incr::Lang::JavaScript);
        assert_eq!(
            n3, 2,
            "only the def + call, NOT the template literal: {out3:?}"
        );
        assert!(
            out3.contains("`call foo now`"),
            "template text must be untouched: {out3:?}"
        );

        // SCRY-111: a Python triple-quoted docstring with an ODD number of internal
        // quotes (e.g. a `5"` measurement) must NOT desync the scanner — the symbol
        // mentioned after it inside the docstring stays untouched. (triple_quote=true.)
        let pydoc = "def foo():\n    \"\"\"Max 5\" then call foo here.\"\"\"\n    foo()\n";
        let (out4, n4) = replace_word(pydoc, "foo", "bar", vyer_incr::Lang::Python);
        assert_eq!(n4, 2, "only def+call, NOT the docstring mention: {out4:?}");
        assert!(
            out4.contains("then call foo here."),
            "triple-quoted docstring must be untouched: {out4:?}"
        );

        // SCRY-112: a Rust raw string `r#"..."#` holds internal `"` verbatim — a symbol
        // mentioned inside it must NOT be renamed (raw_string=true).
        let rs = "fn foo() {}\nfoo();\nlet q = r#\"run \"foo\" here\"#;\n";
        let (out5, n5) = replace_word(rs, "foo", "bar", vyer_incr::Lang::Rust);
        assert_eq!(n5, 2, "only def+call, NOT the raw string: {out5:?}");
        assert!(
            out5.contains("run \"foo\" here"),
            "raw-string text must be untouched: {out5:?}"
        );
    }

    #[test]
    fn parse_diagnostics_covers_common_formats() {
        let blob = "error[E0308]: mismatched types\n  --> src/a.rs:42:5\n\
                    lib/b.dart:7:3: Error: x\n\
                    app/c.ts(9,2): error TS1\n\
                    File \"scripts/d.py\", line 11, in f\n\
                    \x20   at g (src/e.js:3:8)\n\
                    ./f.go:4: undefined\n\
                    see https://example.com:443/x for more\n";
        let refs = parse_diagnostics(blob);
        let has = |p: &str, l: u32| refs.iter().any(|d| d.path == p && d.line == l);
        assert!(has("src/a.rs", 42), "rust :col: {refs:?}");
        assert!(has("lib/b.dart", 7), "dart trailing-colon: {refs:?}");
        assert!(has("app/c.ts", 9), "tsc paren: {refs:?}");
        assert!(has("scripts/d.py", 11), "python: {refs:?}");
        assert!(has("src/e.js", 3), "js stack: {refs:?}");
        assert!(has("./f.go", 4), "go: {refs:?}");
        // a URL (host:port) must NOT be mistaken for a path:line ref.
        assert!(
            !refs.iter().any(|d| d.path.contains("example.com")),
            "URL leaked as a ref: {refs:?}"
        );
        // SCRY-133: severity + message are extracted (structured), incl. Rust's
        // two-line form where the message precedes the `-->` location line.
        let rust = refs.iter().find(|d| d.path == "src/a.rs").unwrap();
        assert_eq!(
            rust.severity.as_deref(),
            Some("error"),
            "rust sev: {refs:?}"
        );
        assert_eq!(
            rust.message.as_deref(),
            Some("mismatched types"),
            "rust msg carried from the error[..] line: {refs:?}"
        );
        let ts = refs.iter().find(|d| d.path == "app/c.ts").unwrap();
        assert_eq!(ts.severity.as_deref(), Some("error"), "tsc sev: {refs:?}");
    }

    #[test]
    fn diagnose_mode_maps_errors_to_code() {
        let engine = engine_with(&[("src/a.rs", "fn run() {\n    bad();\n}\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "error[E0425]: cannot find value `bad`\n  --> src/a.rs:2:5".into(),
                mode: "diagnose".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("src/a.rs#run"),
            "should locate the enclosing symbol: {out}"
        );
        assert!(out.contains(">> 2:"), "should mark the failing line: {out}");
        // SCRY-133: the structured header carries severity + enclosing symbol + message.
        assert!(
            out.contains("src/a.rs:2 error in run"),
            "structured header (file:line severity in symbol): {out}"
        );
        assert!(
            out.contains("cannot find value"),
            "header carries the message: {out}"
        );
    }

    #[test]
    fn project_info_detects_stacks_and_commands() {
        let dir = std::env::temp_dir().join(format!("vyer_proj_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pubspec.yaml"),
            "name: x\ndependencies:\n  flutter:\n    sdk: flutter\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("package.json"),
            "{\"scripts\":{\"test\":\"jest\",\"build\":\"tsc\"}}",
        )
        .unwrap();
        std::fs::write(dir.join("Makefile"), "build:\n\tgo build\n.PHONY: build\n").unwrap();
        let engine = Engine {
            config: EngineConfig::new(dir.clone()),
            db: Mutex::new(Db::new()),
            seen: Mutex::new(HashSet::new()),
            audit: Mutex::new(Vec::new()),
            token_index: Mutex::new(None),
            semantic_index: Mutex::new(None),
            history: Mutex::new(Vec::new()),
        };
        let info = engine.project_info();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(info.contains("dart (flutter)"), "flutter: {info}");
        assert!(info.contains("scripts:"), "node scripts: {info}");
        assert!(info.contains("make build"), "make target: {info}");
        assert!(
            !info.contains(".PHONY"),
            "make directive must be skipped: {info}"
        );
        assert!(info.contains("mode=diagnose"), "bridge to diagnose: {info}");
    }

    #[test]
    fn project_info_discovers_subdir_manifests() {
        // SCRY-125 (#5): a monorepo keeps manifests in service subdirs; project_info
        // must surface them, not report "no build manifest".
        let engine = engine_with(&[
            ("services/api/go.mod", "module api\n"),
            ("apps/web/package.json", "{}\n"),
            ("src/main.rs", "fn main() {}\n"),
        ]);
        let info = engine.project_info();
        assert!(
            info.contains("monorepo packages"),
            "should list subdir manifests: {info}"
        );
        assert!(info.contains("services/api/go.mod"), "go.mod: {info}");
        assert!(
            info.contains("apps/web/package.json"),
            "package.json: {info}"
        );
    }

    #[test]
    fn import_resolves_symbol_and_builds_statement() {
        let engine = engine_with(&[
            ("src/models/user.ts", "export class User {}\n"),
            ("src/screens/home.ts", "export function home() {}\n"),
        ]);
        let with_target = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "User".into(),
                path: Some("src/screens/home.ts".into()),
                detail: "import".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            with_target.contains("import { User } from '../models/user'"),
            "exact relative ts import: {with_target}"
        );
        let unknown = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "NoSuchSymbol".into(),
                detail: "import".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            unknown.contains("not defined in any indexed file"),
            "honest unknown: {unknown}"
        );
    }

    fn base_query() -> Query {
        Query {
            q: String::new(),
            path: None,
            mode: "auto".into(),
            detail: "tree".into(),
            path_scope: vec![],
            lang: None,
            lines: None,
            all_of: vec![],
            any_of: vec![],
            none_of: vec![],
            k: 8,
        }
    }

    #[test]
    fn tree_lists_and_filters() {
        let engine = engine_with(&[
            ("src/a.rs", "fn a() {}\n"),
            ("src/sub/b.rs", "fn b() {}\n"),
            ("docs/readme.md", "# hi\n"),
        ]);
        let db = engine.db.lock().unwrap();

        // whole repo: all three files appear, nested under their dirs
        let all = &engine.tree_spans(&db, &base_query())[0].text;
        assert!(all.starts_with("3 files\n"), "got: {all}");
        assert!(all.contains("a.rs") && all.contains("b.rs") && all.contains("readme.md"));
        assert!(all.contains("sub/"), "nested dir should render: {all}");

        // path prefix = src → only the two src files
        let q_src = Query {
            path: Some("src".into()),
            ..base_query()
        };
        let src = &engine.tree_spans(&db, &q_src)[0].text;
        assert!(src.starts_with("2 files\n"), "got: {src}");
        assert!(!src.contains("readme.md"));

        // substring filter q = "b.rs"
        let q_sub = Query {
            q: "b.rs".into(),
            ..base_query()
        };
        let sub = &engine.tree_spans(&db, &q_sub)[0].text;
        assert!(sub.starts_with("1 files\n"), "got: {sub}");
        assert!(sub.contains("b.rs") && !sub.contains("a.rs"));
    }

    #[test]
    fn diff_reports_session_changes() {
        let engine = engine_with(&[("src/a.rs", "fn a() {}\n")]);

        // empty history → a friendly message, never an empty/error result.
        {
            let db = engine.db.lock().unwrap();
            let none = &engine.diff_spans(&db, &base_query())[0].text;
            assert!(
                none.contains("no edits applied this session"),
                "got: {none}"
            );
        }

        // simulate a code_apply: snapshot the pre-edit text (as SP-6 does), then
        // mutate the warm core to the post-edit text.
        engine.history.lock().unwrap().push(vec![(
            "src/a.rs".to_string(),
            Some("fn a() {}\n".to_string()),
        )]);
        engine
            .db
            .lock()
            .unwrap()
            .set_text("src/a.rs", "fn a() -> u8 { 0 }\n");

        {
            let db = engine.db.lock().unwrap();
            let d = &engine.diff_spans(&db, &base_query())[0].text;
            assert!(d.contains("(src/a.rs)"), "path summary missing: {d}");
            assert!(d.contains("```diff"), "should be fenced: {d}");
            assert!(
                d.contains("-fn a() {}") && d.contains("+fn a() -> u8 { 0 }"),
                "diff body wrong: {d}"
            );
        }

        // net-zero: revert the warm core to the original → nothing to show.
        engine
            .db
            .lock()
            .unwrap()
            .set_text("src/a.rs", "fn a() {}\n");
        let db = engine.db.lock().unwrap();
        let z = &engine.diff_spans(&db, &base_query())[0].text;
        assert!(
            z.contains("net-zero") || z.contains("none match"),
            "expected net-zero message: {z}"
        );
    }

    #[test]
    fn diff_summarizes_created_files() {
        // SCRY-123 (#7): a file CREATED this session is summarized in detail=diff,
        // not dumped as a full +file (which turned diff into a repo dump).
        let engine = engine_with(&[("keep.rs", "fn keep() {}\n")]);
        engine
            .history
            .lock()
            .unwrap()
            .push(vec![("new.rs".to_string(), None)]);
        engine
            .db
            .lock()
            .unwrap()
            .set_text("new.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        let db = engine.db.lock().unwrap();
        let d = &engine.diff_spans(&db, &base_query())[0].text;
        assert!(
            d.contains("created NEW file (+3 lines)"),
            "should summarize created file: {d}"
        );
        assert!(!d.contains("fn b()"), "must not dump the file body: {d}");
    }

    #[test]
    fn batch_queries_report_per_query_attribution() {
        let engine = engine_with(&[("src/a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let req = CodeRequest {
            queries: vec![
                Query {
                    q: "alpha".into(),
                    mode: "lexical".into(),
                    detail: "locate".into(),
                    ..base_query()
                },
                Query {
                    q: "zzzznotfound".into(),
                    mode: "lexical".into(),
                    detail: "locate".into(),
                    ..base_query()
                },
            ],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("per-query found:"),
            "no attribution note: {out}"
        );
        assert!(out.contains("q0 `alpha`\u{2192}1"), "q0 count wrong: {out}");
        assert!(
            out.contains("q1 `zzzznotfound`\u{2192}0"),
            "q1 count wrong: {out}"
        );

        // A single-query request must NOT carry the note (no batching to attribute).
        let single = CodeRequest {
            queries: vec![Query {
                q: "alpha".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            !engine.code(&single).contains("per-query found:"),
            "single query should not be attributed"
        );
    }

    #[test]
    fn boolean_query_attribution_labels_by_terms() {
        // NEW-D / SCRY-124: a boolean query is labeled by its TERMS in the per-query
        // found note, not by its detail value.
        let engine = engine_with(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let req = CodeRequest {
            queries: vec![
                Query {
                    q: "alpha".into(),
                    mode: "lexical".into(),
                    detail: "locate".into(),
                    ..base_query()
                },
                Query {
                    all_of: vec!["fn".into(), "beta".into()],
                    detail: "count".into(),
                    ..base_query()
                },
            ],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("per-query found:"), "no attribution: {out}");
        assert!(
            out.contains("q1 `all[fn,beta]`"),
            "boolean query must be labeled by its terms: {out}"
        );
    }

    #[test]
    fn batch_mixed_detail_modes_do_not_interfere() {
        // A batch mixing a `count` query (its own branch) with a `snippet` search
        // (the fused path) — each must produce its own correct output in one call.
        let engine = engine_with(&[("a.rs", "fn validate() {}\nfn other() {}\n")]);
        let req = CodeRequest {
            queries: vec![
                Query {
                    q: "fn".into(),
                    mode: "lexical".into(),
                    detail: "count".into(),
                    ..base_query()
                },
                Query {
                    q: "validate".into(),
                    mode: "structural".into(),
                    detail: "snippet".into(),
                    ..base_query()
                },
            ],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("matches on"),
            "count query output missing: {out}"
        );
        assert!(
            out.contains("validate"),
            "snippet query output missing: {out}"
        );
        // per-query attribution still present for the 2-query batch.
        assert!(
            out.contains("per-query found:"),
            "attribution missing: {out}"
        );
    }

    #[test]
    fn batch_dedups_overlapping_spans() {
        // SCRY-064: two queries in one batch that match the SAME span must not
        // duplicate it in the output (token waste).
        let engine = engine_with(&[("a.rs", "pub fn shared_target() {\n    let x = 1;\n}\n")]);
        let req = CodeRequest {
            queries: vec![
                Query {
                    q: "shared_target".into(),
                    mode: "structural".into(),
                    detail: "snippet".into(),
                    ..base_query()
                },
                Query {
                    q: "shared".into(),
                    mode: "structural".into(),
                    detail: "snippet".into(),
                    ..base_query()
                },
            ],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let count = out.matches("#shared_target@").count();
        assert_eq!(
            count, 1,
            "an overlapping batch span should appear once, got {count}: {out}"
        );
    }

    #[test]
    fn every_code_span_is_marked_untrusted() {
        // Rule §8 (indirect-injection defense): EVERY returned code span must carry
        // `source=UNTRUSTED`, not just the first — lock the count equality so a new
        // detail mode can't ship spans without the marker.
        let engine = engine_with(&[("a.rs", "fn one() {}\nfn two() {}\nfn three() {}\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "fn".into(),
                mode: "lexical".into(),
                detail: "snippet".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let spans = out.matches("\u{27E6}span\u{27E7}").count();
        let marks = out.matches("source=UNTRUSTED").count();
        assert!(spans >= 3, "expected multiple spans: {out}");
        assert_eq!(spans, marks, "every span must be UNTRUSTED-marked: {out}");
    }

    #[test]
    fn identical_queries_produce_identical_output() {
        // Rule §9: the core is deterministic — the same query returns byte-identical
        // output across runs (agent trust + prefix caching depend on it). Covers
        // the ranking-sensitive fused (auto) and graph (refs/context) paths.
        let engine = engine_with(&[
            ("a.rs", "fn alpha() {}\nfn beta() {\n    alpha()\n}\n"),
            ("b.rs", "fn gamma() {\n    alpha();\n    beta()\n}\n"),
        ]);
        for (q, mode, detail) in [
            ("alpha", "auto", "snippet"),
            ("alpha", "graph", "refs"),
            ("beta", "auto", "context"),
        ] {
            let req = CodeRequest {
                queries: vec![Query {
                    q: q.into(),
                    mode: mode.into(),
                    detail: detail.into(),
                    ..base_query()
                }],
                budget_tokens: 8000,
                exclude_seen: false,
            };
            let a = engine.code(&req);
            let b = engine.code(&req);
            assert_eq!(a, b, "non-deterministic output for {q}/{mode}/{detail}");
        }
    }

    #[test]
    fn concurrent_reads_are_safe_and_consistent() {
        // Rule §9 (concurrency): queries are `&self` over an interior-mutable warm
        // core — many threads can query the SAME engine at once without a data race,
        // panic, or inconsistent result. Locks the shared-read safety the resident
        // daemon relies on.
        use std::sync::Arc;
        use std::thread;
        let engine = Arc::new(engine_with(&[
            ("a.rs", "fn alpha() {}\nfn beta() {\n    alpha()\n}\n"),
            ("b.rs", "fn gamma() {\n    alpha()\n}\n"),
        ]));
        // a stable reference result computed single-threaded.
        let want = {
            let req = CodeRequest {
                queries: vec![Query {
                    q: "alpha".into(),
                    mode: "graph".into(),
                    detail: "refs".into(),
                    ..base_query()
                }],
                budget_tokens: 8000,
                exclude_seen: false,
            };
            engine.code(&req)
        };
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let eng = Arc::clone(&engine);
                let want = want.clone();
                thread::spawn(move || {
                    for _ in 0..50 {
                        let req = CodeRequest {
                            queries: vec![Query {
                                q: "alpha".into(),
                                mode: "graph".into(),
                                detail: "refs".into(),
                                ..base_query()
                            }],
                            budget_tokens: 8000,
                            exclude_seen: false,
                        };
                        assert_eq!(eng.code(&req), want, "concurrent read diverged");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("a reader thread panicked");
        }
    }

    #[test]
    fn nl_symbol_mention_recall() {
        // SCRY-057: a DISTINCTIVE symbol name mentioned in prose is surfaced; a
        // generic stop-word (`files`) is not (would be noise in any repo).
        let engine = engine_with(&[(
            "a.rs",
            "pub fn squelch(x: u8) -> u8 {\n    x\n}\npub fn files() -> u8 {\n    0\n}\n",
        )]);
        let db = engine.db.lock().unwrap();
        let mut cands = HashMap::new();
        let files = vec!["a.rs".to_string()];

        let q1 = Query {
            q: "how does squelch decide what to drop".into(),
            ..base_query()
        };
        let m1 = engine.symbol_mention_ids(&db, &q1, &files, &mut cands);
        assert!(
            m1.iter().any(|id| id.contains("squelch")),
            "distinctive prose mention should be surfaced: {m1:?}"
        );

        let q2 = Query {
            q: "read the files from somewhere".into(),
            ..base_query()
        };
        let m2 = engine.symbol_mention_ids(&db, &q2, &files, &mut cands);
        assert!(
            !m2.iter().any(|id| id.contains("#files@")),
            "generic stop-word should not pull in a symbol: {m2:?}"
        );
    }

    #[test]
    fn refs_excludes_comment_and_string_mentions() {
        // SCRY-059: `refs` must count only CODE references — a name in a comment or
        // string literal is not a reference (same precision as context/impact).
        let engine = engine_with(&[(
            "m.rs",
            "fn target() {}\nfn real_caller() {\n    target()\n}\n// target in a comment\nfn other() {\n    let s = \"target in a string\";\n}\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "graph".into(),
                detail: "refs".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("refs=1"),
            "only the real call should count, not comment/string: {out}"
        );
        assert!(
            !out.contains("in a comment") && !out.contains("in a string"),
            "comment/string lines must not appear as references: {out}"
        );
    }

    #[test]
    fn refs_excludes_python_hash_comments() {
        // SCRY-060: `#` starts a comment in Python/Ruby — a name appearing only
        // there is not a reference (the C-style `//`/`/* */` skip missed it).
        let engine = engine_with(&[(
            "p.py",
            "def target():\n    pass\ndef caller():\n    target()\n# target in a comment\nx = \"target str\"\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "graph".into(),
                detail: "refs".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("refs=1"),
            "a Python `#` comment / string must not count as a reference: {out}"
        );
    }

    #[test]
    fn refs_finds_cross_file_after_prune() {
        // SCRY-061: pruning the reference scan to name-containing files must not
        // drop a reference in a DIFFERENT file from the definition (a reference
        // can only exist in a file that contains the name — sound prune).
        let engine = engine_with(&[
            ("a.rs", "pub fn shared_helper() {}\n"),
            ("b.rs", "fn caller() {\n    shared_helper()\n}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "shared_helper".into(),
                mode: "graph".into(),
                detail: "refs".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("refs=1") && out.contains("b.rs"),
            "cross-file reference dropped by the prune: {out}"
        );
    }

    #[test]
    fn context_finds_cross_file_caller_after_prune() {
        // SCRY-062: pruning the caller scan to target-containing files must still
        // find a caller in a DIFFERENT file from the definition.
        let engine = engine_with(&[
            ("a.rs", "pub fn widget() {}\n"),
            ("b.rs", "fn user() {\n    widget()\n}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "widget".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("1 caller") && out.contains("b.rs#user"),
            "cross-file caller dropped by the prune: {out}"
        );
    }

    #[test]
    fn large_file_outline_caps_to_budget() {
        // SCRY-063: a huge file's outline must show as many signatures as the
        // budget allows + a remainder note — never get packed out to an empty
        // result (one oversized span).
        let body: String = (0..800)
            .map(|i| format!("pub fn func_{i}() {{}}\n"))
            .collect();
        let engine = engine_with(&[("big.rs", body.as_str())]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "".into(),
                path: Some("big.rs".into()),
                detail: "outline".into(),
                ..base_query()
            }],
            budget_tokens: 2000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("func_0"),
            "should show the first symbols: {out}"
        );
        assert!(
            out.contains("more symbols"),
            "should note the truncated remainder: {out}"
        );
        assert!(
            !out.contains("used=0 "),
            "a big-file outline must not be packed out to empty: {out}"
        );
    }

    #[test]
    fn unknown_mode_warns_but_still_serves() {
        // SCRY-058: a typo'd mode falls back to auto AND tells the agent its param
        // was ignored; a valid mode produces no such note.
        let engine = engine_with(&[("a.rs", "fn alpha() {}\n")]);
        let mk = |mode: &str| CodeRequest {
            queries: vec![Query {
                q: "alpha".into(),
                mode: mode.into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&mk("lexcial"));
        assert!(
            out.contains("unknown mode") && out.contains("lexcial"),
            "a typo'd mode should warn: {out}"
        );
        assert!(
            out.contains("alpha"),
            "should still serve results via the auto fallback: {out}"
        );
        assert!(
            !engine.code(&mk("lexical")).contains("unknown mode"),
            "a valid mode must not warn"
        );
        // SCRY-058 also covers `detail`: a typo'd detail warns, valid stays silent.
        let bad_detail = CodeRequest {
            queries: vec![Query {
                q: "alpha".into(),
                mode: "lexical".into(),
                detail: "snippset".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&bad_detail).contains("unknown detail"),
            "a typo'd detail should warn"
        );
    }

    #[test]
    fn lexical_substring_query_keeps_recall() {
        // `confiden` is a substring of `low_confidence` but never a standalone
        // identifier — the whole-identifier inverted index must still surface it
        // (SCRY-038), matching what a plain grep would find.
        let engine = engine_with(&[("src/a.rs", "let low_confidence = true;\nfn other() {}\n")]);
        let hit = CodeRequest {
            queries: vec![Query {
                q: "confiden".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&hit);
        assert!(
            out.contains("src/a.rs"),
            "substring query lost recall: {out}"
        );
        assert!(
            !out.contains("PATTERN_NO_MATCH"),
            "substring query was pruned to empty: {out}"
        );

        // A substring present in NO identifier still prunes to nothing — the index
        // win is preserved, no false positives.
        let miss = CodeRequest {
            queries: vec![Query {
                q: "zzqqxx".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&miss).contains("PATTERN_NO_MATCH"),
            "absent substring should yield no match"
        );
    }

    #[test]
    fn multiword_literal_query_prunes_but_keeps_recall() {
        let engine = engine_with(&[
            ("a.rs", "fn validate_token() {}\n"),
            ("b.rs", "fn other() {}\n"),
        ]);
        // "fn validate" is a pure literal → pruned to files with the `validate`
        // token, still finds the real match (a.rs), excludes b.rs.
        let lit = CodeRequest {
            queries: vec![Query {
                q: "fn validate".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&lit);
        assert!(out.contains("a.rs"), "literal phrase lost recall: {out}");
        assert!(!out.contains("b.rs"), "literal phrase over-matched: {out}");

        // A regex with alternation must NOT be AND-pruned — it matches either side.
        let rx = CodeRequest {
            queries: vec![Query {
                q: "validate|other".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let rout = engine.code(&rx);
        assert!(
            rout.contains("a.rs") && rout.contains("b.rs"),
            "regex alternation must match both files (no unsound AND-prune): {rout}"
        );
    }

    #[test]
    fn literal_phrase_prune_reduces_candidates() {
        // Structural guard for SCRY-047: the prune must actually shrink the
        // candidate file set (not silently fall back to a full scan).
        assert_eq!(
            literal_phrase_tokens("fn parse"),
            Some(vec!["parse".to_string()])
        );
        assert_eq!(
            literal_phrase_tokens("a|b"),
            None,
            "regex must not be pruned"
        );
        assert_eq!(literal_phrase_tokens("x y"), None, "no ≥3-char token");
        // `.` (any-char) keeps both tokens required — method-call searches prune.
        assert_eq!(
            literal_phrase_tokens("obj.method"),
            Some(vec!["obj".to_string(), "method".to_string()])
        );
        // optional-making metachars fall through to a full scan (sound).
        assert_eq!(
            literal_phrase_tokens("parse?"),
            None,
            "? makes a token optional"
        );
        assert_eq!(
            literal_phrase_tokens("a*b"),
            None,
            "* makes a token optional"
        );

        let engine = engine_with(&[
            ("a.rs", "fn validate_token() {}\n"),
            ("b.rs", "fn other() {}\n"),
            ("c.rs", "fn more() {}\n"),
        ]);
        let db = engine.db.lock().unwrap();
        let q = Query {
            q: "fn validate".into(),
            ..base_query()
        };
        let scoped = engine.scoped_files(&db, &q);
        let pruned = engine.pruned_lex_files(&db, &q, &scoped);
        assert_eq!(
            pruned,
            vec!["a.rs".to_string()],
            "literal phrase should prune to only the token's files, got: {pruned:?}"
        );
    }

    #[test]
    fn regex_prefix_prune_preserves_recall() {
        // SCRY-065 soundness: the extraction + the file-prune must never drop a
        // match. Extraction edges first:
        assert_eq!(
            regex_required_prefix(r"validate_\w+"),
            Some("validate_".into())
        );
        assert_eq!(
            regex_required_prefix("error.*handler"),
            Some("error".into())
        );
        assert_eq!(
            regex_required_prefix("abc?def"),
            None,
            "a `?`-optional last char must NOT be pruned by"
        );
        assert_eq!(
            regex_required_prefix("foo*bar"),
            None,
            "a `*`-optional last char → run too short"
        );
        assert_eq!(
            regex_required_prefix("fo+bar"),
            None,
            "`+` keeps last char but run is <3"
        );
        assert_eq!(regex_required_prefix("(a|b)cd"), None, "alternation start");
        assert_eq!(
            regex_required_prefix("validate|other"),
            None,
            "TOP-LEVEL alternation must NOT prune (the other branch lacks the prefix)"
        );
        assert_eq!(
            regex_required_prefix("validate_(a|b)"),
            Some("validate_".into()),
            "alternation inside a LATER group is fine — the prefix is still required"
        );
        assert_eq!(regex_required_prefix(r"\d+x"), None, "metachar start");
        assert_eq!(regex_required_prefix("^foobar.*"), Some("foobar".into()));
        // more sound constructs where the prefix stays required:
        assert_eq!(regex_required_prefix("validate$"), Some("validate".into()));
        assert_eq!(
            regex_required_prefix("validate[0-9]"),
            Some("validate".into())
        );
        assert_eq!(
            regex_required_prefix("validate(_x)?"),
            Some("validate".into())
        );

        // End-to-end: a regex finds the SAME files with the prune as without — the
        // prune only narrows the candidate set soundly (both `validate_*` survive).
        let engine = engine_with(&[
            ("a.rs", "fn validate_token() {}\n"),
            ("b.rs", "fn validate_id() {}\n"),
            ("c.rs", "fn unrelated() {}\n"),
        ]);
        let db = engine.db.lock().unwrap();
        let q = Query {
            q: r"validate_\w+".into(),
            ..base_query()
        };
        let scoped = engine.scoped_files(&db, &q);
        let pruned = engine.pruned_lex_files(&db, &q, &scoped);
        assert!(
            pruned.contains(&"a.rs".to_string()) && pruned.contains(&"b.rs".to_string()),
            "both validate_* files must survive the prune (recall): {pruned:?}"
        );
        assert!(
            !pruned.contains(&"c.rs".to_string()),
            "the unrelated file should be pruned (efficiency): {pruned:?}"
        );
    }

    #[test]
    fn regex_multi_literal_prune_is_sound() {
        // SCRY-066: extract ALL required literals from a FLAT regex — and NEVER
        // from a construct whose contents aren't literal text (recall safety).
        assert_eq!(
            regex_required_literals("error.*handler"),
            vec!["error", "handler"]
        );
        assert_eq!(
            regex_required_literals(r"get\w+handler"),
            vec!["get", "handler"]
        );
        assert_eq!(
            regex_required_literals("abc?def"),
            vec!["def"],
            "a `?`-optional last char drops abc→ab (<3)"
        );
        assert_eq!(
            regex_required_literals("abcd*efg"),
            vec!["abc", "efg"],
            "a `*`-optional last char drops d"
        );
        assert_eq!(
            regex_required_literals("^error.*$"),
            vec!["error"],
            "anchors stripped"
        );
        // non-flat constructs whose contents are NOT required literals → empty:
        assert!(
            regex_required_literals("[abc]xyz").is_empty(),
            "char class is one-OF, not the literal abc"
        );
        assert!(
            regex_required_literals("ab{100}cd").is_empty(),
            "counted quantifier is a count, not the literal 100"
        );
        assert!(regex_required_literals("(foo)bar").is_empty(), "group");
        assert!(regex_required_literals("foo|bar").is_empty(), "alternation");

        // e2e recall: `validate.*token` matches only files with BOTH literals; the
        // prune (validate ∩ token) keeps exactly those, drops validate-only — and
        // those dropped files genuinely can't match (no `token` after `validate`).
        let engine = engine_with(&[
            ("both.rs", "fn validate_the_token() {}\n"),
            ("validonly.rs", "fn validate_nothing() {}\n"),
            ("none.rs", "fn other() {}\n"),
        ]);
        let db = engine.db.lock().unwrap();
        let q = Query {
            q: "validate.*token".into(),
            ..base_query()
        };
        let scoped = engine.scoped_files(&db, &q);
        let pruned = engine.pruned_lex_files(&db, &q, &scoped);
        assert_eq!(
            pruned,
            vec!["both.rs".to_string()],
            "only the file with BOTH literals survives: {pruned:?}"
        );
    }

    #[test]
    fn structural_prune_keeps_substring_name_matches() {
        // SCRY-048: structural search is pruned to the lexical candidate files for
        // a plain-ident query. A symbol whose NAME merely CONTAINS the query must
        // still be found — its file holds the name identifier, so it's in the set.
        let engine = engine_with(&[
            ("a.rs", "fn validate_token() {}\n"),
            ("b.rs", "fn unrelated() {}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "validate".into(),
                mode: "structural".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("validate_token"),
            "substring-name structural match lost by the prune: {out}"
        );
        assert!(
            !out.contains("unrelated"),
            "should not match unrelated: {out}"
        );
    }

    #[test]
    fn empty_query_does_not_dump_the_repo() {
        // SCRY-049: an empty search term must NOT match every symbol/line.
        let engine = engine_with(&[("a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "".into(),
                mode: "auto".into(),
                detail: "snippet".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("PATTERN_NO_MATCH"),
            "empty query must not match everything: {out}"
        );
        assert!(
            !out.contains("alpha") && !out.contains("beta"),
            "empty query leaked symbols: {out}"
        );
    }

    #[test]
    fn exclude_seen_pages_all_without_repeats() {
        // SCRY-023: paging with exclude_seen must cover every match exactly once
        // and terminate — no drops, no repeats.
        let engine = engine_with(&[(
            "a.rs",
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\nfn delta() {}\n",
        )]);
        let mut seen: HashSet<String> = HashSet::new();
        let mut pages = 0;
        loop {
            let req = CodeRequest {
                queries: vec![Query {
                    q: "fn".into(),
                    mode: "lexical".into(),
                    detail: "locate".into(),
                    k: 2,
                    ..base_query()
                }],
                budget_tokens: 8000,
                exclude_seen: true,
            };
            let out = engine.code(&req);
            if out.contains("PATTERN_NO_MATCH") {
                break;
            }
            let span_lines: Vec<String> = out
                .lines()
                .filter(|l| l.starts_with("\u{27E6}span\u{27E7}"))
                .map(|l| l.to_string())
                .collect();
            assert!(
                !span_lines.is_empty(),
                "non-error page with no spans: {out}"
            );
            for l in span_lines {
                assert!(seen.insert(l.clone()), "span repeated across pages: {l}");
            }
            pages += 1;
            assert!(pages <= 10, "paging did not terminate");
        }
        // all four functions surfaced across the pages, none repeated.
        assert!(
            seen.len() >= 4,
            "paging missed matches (found {}): {seen:?}",
            seen.len()
        );
    }

    #[test]
    fn structural_paging_covers_all_symbols() {
        // structural_ids returns the FULL match set (no per-file cap), so paging a
        // symbol-name query covers every match — complements the SCRY-054 lexical
        // fix, confirming the common (symbol) paging case is complete too.
        let engine = engine_with(&[(
            "a.rs",
            "fn parse_a() {}\nfn parse_b() {}\nfn parse_c() {}\n",
        )]);
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..6 {
            let req = CodeRequest {
                queries: vec![Query {
                    q: "parse".into(),
                    mode: "structural".into(),
                    detail: "locate".into(),
                    k: 1,
                    ..base_query()
                }],
                budget_tokens: 8000,
                exclude_seen: true,
            };
            let out = engine.code(&req);
            if out.contains("PATTERN_NO_MATCH") {
                break;
            }
            for l in out
                .lines()
                .filter(|l| l.starts_with("\u{27E6}span\u{27E7}"))
            {
                seen.insert(l.to_string());
            }
        }
        assert!(
            seen.len() >= 3,
            "structural paging missed symbols (found {}): {seen:?}",
            seen.len()
        );
    }

    #[test]
    fn code_request_accepts_single_query_and_string_sugar() {
        use serde_json::json;
        // single-query SUGAR: top-level query fields, no `queries` wrapper.
        let r: CodeRequest =
            serde_json::from_value(json!({"q":"validateToken","detail":"locate"})).unwrap();
        assert_eq!(r.queries.len(), 1);
        assert_eq!(r.queries[0].q, "validateToken");
        assert_eq!(r.queries[0].detail, "locate");
        assert_eq!(r.queries[0].mode, "auto"); // defaults still apply
        assert_eq!(r.budget_tokens, 8000);
        // a bare STRING is one query.
        let r: CodeRequest = serde_json::from_value(json!("foo")).unwrap();
        assert_eq!(r.queries[0].q, "foo");
        // canonical batch, with a bare-string item mixed in + top-level options.
        let r: CodeRequest = serde_json::from_value(
            json!({"queries":[{"q":"a"}, "b"], "budget_tokens": 1234, "exclude_seen": true}),
        )
        .unwrap();
        assert_eq!(r.queries.len(), 2);
        assert_eq!(r.queries[1].q, "b");
        assert_eq!(r.budget_tokens, 1234);
        assert!(r.exclude_seen);
        // an unparseable shape yields a TOOL-AUTHORED message, not a raw serde dump.
        let err = serde_json::from_value::<CodeRequest>(json!(42)).unwrap_err();
        assert!(err.to_string().contains("\"q\""), "actionable hint: {err}");
    }

    #[test]
    fn apply_request_accepts_single_edit_sugar() {
        use serde_json::json;
        // single-edit SUGAR.
        let r: ApplyRequest =
            serde_json::from_value(json!({"locator":"src/x.rs#foo","new_body":"fn foo(){}"}))
                .unwrap();
        assert_eq!(r.edits.len(), 1);
        assert_eq!(r.edits[0].locator, "src/x.rs#foo");
        // canonical batch + undo forms unchanged.
        let r: ApplyRequest =
            serde_json::from_value(json!({"edits":[{"locator":"a#b"}], "dry_run": true})).unwrap();
        assert_eq!(r.edits.len(), 1);
        assert!(r.dry_run);
        let r: ApplyRequest = serde_json::from_value(json!({"undo":2})).unwrap();
        assert_eq!(r.undo, Some(2));
    }

    #[test]
    fn apply_missing_locator_is_tool_authored() {
        // SCRY-128: a missing locator returns an actionable Err, not serde's
        // `missing field 'locator'`. (Writes are disabled here, but the locator
        // guard must come BEFORE that check is irrelevant — we exercise it via an
        // allow-writes engine so the guard is reached.)
        let mut engine = engine_with(&[("src/x.rs", "fn foo() {}\n")]);
        engine.config.allow_writes = true;
        let req = ApplyRequest {
            edits: vec![Edit {
                locator: String::new(),
                anchor: Some("foo".into()),
                replace: Some("bar".into()),
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        };
        let err = engine.code_apply(&req).unwrap_err();
        assert!(err.contains("locator"), "{err}");
        assert!(err.contains("PATH#SYMBOL"), "explains the format: {err}");
    }

    #[test]
    fn derive_run_tasks_seeds_safe_defaults() {
        // SCRY-141: --allow-run alone yields a safe build/test/lint allowlist from
        // the manifests, never a mutating task (fmt) or a long-running one (run).
        let dir = std::env::temp_dir().join(format!("vyer_runtasks_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let tasks = derive_run_tasks(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            tasks.get("test").map(|v| v.join(" ")).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            tasks.get("lint").map(|v| v.join(" ")).as_deref(),
            Some("cargo clippy")
        );
        assert!(tasks.contains_key("check") && tasks.contains_key("build"));
        // never a mutating/long-running default.
        assert!(!tasks.values().any(|v| v.join(" ").contains("fmt")));
        assert!(!tasks.contains_key("run"));
    }

    #[test]
    fn code_run_executes_allowlisted_task_only() {
        // SCRY-140: code_run runs an OPERATOR-allowlisted task by NAME, gated by
        // --allow-run; it never accepts a command string (Rule §3).
        let mut engine = engine_with(&[("a.rs", "fn a() {}\n")]);
        // disabled by default → refused.
        let off = engine.code_apply(&ApplyRequest {
            edits: vec![],
            dry_run: false,
            undo: None,
            run: Some("test".into()),
        });
        assert!(
            off.unwrap_err().contains("--allow-run"),
            "run must be gated by --allow-run"
        );
        // enable + register an allowlisted task (a portable no-op: `true`).
        engine.config.allow_run = true;
        engine
            .config
            .run_tasks
            .insert("ok".into(), vec!["true".into()]);
        let out = engine
            .code_apply(&ApplyRequest {
                edits: vec![],
                dry_run: false,
                undo: None,
                run: Some("ok".into()),
            })
            .expect("allowlisted task runs");
        assert!(out.contains("run(true)=ok"), "structured run report: {out}");
        // an unknown task is refused and lists the allowlist — never executes.
        let unknown = engine.code_apply(&ApplyRequest {
            edits: vec![],
            dry_run: false,
            undo: None,
            run: Some("rm -rf /".into()),
        });
        let e = unknown.unwrap_err();
        assert!(
            e.contains("unknown run task") && e.contains("ok"),
            "unknown task lists the allowlist, runs nothing: {e}"
        );
    }

    #[test]
    fn rename_onto_existing_name_is_refused() {
        // SCRY-139 (guiding master): renaming onto a name that already exists would
        // merge two distinct symbols — refuse and name the clash.
        let mut engine = engine_with(&[("src/a.rs", "fn alpha() {}\nfn beta() {}\n")]);
        engine.config.allow_writes = true;
        let mk = |force: bool| ApplyRequest {
            edits: vec![Edit {
                locator: "src/a.rs#alpha".into(),
                rename: Some("beta".into()),
                force,
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        };
        let err = engine.code_apply(&mk(false)).unwrap_err();
        assert!(
            err.contains("already exists") && err.contains("beta"),
            "{err}"
        );
        // force overrides.
        let forced = engine.code_apply(&mk(true));
        assert!(
            forced.is_ok() || !forced.unwrap_err().contains("already exists"),
            "force:true must override the collision guard"
        );
        // renaming to a FREE name is fine.
        let free = engine.code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/a.rs#alpha".into(),
                rename: Some("gamma".into()),
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        });
        assert!(
            free.is_ok() || !free.unwrap_err().contains("already exists"),
            "rename to a free name must not be refused"
        );
    }

    #[test]
    fn new_body_edit_reports_blast_radius() {
        // SCRY-138 (guiding master): replacing a symbol's body surfaces its callers
        // so the agent verifies them (catches the changed-signature/broke-callers
        // mistake before it ships).
        let mut engine = engine_with(&[
            ("src/a.rs", "pub fn helper() -> i32 {\n    1\n}\n"),
            ("src/b.rs", "fn use_it() -> i32 {\n    helper()\n}\n"),
        ]);
        engine.config.allow_writes = true;
        let req = ApplyRequest {
            edits: vec![Edit {
                locator: "src/a.rs#helper".into(),
                new_body: Some("pub fn helper() -> i32 {\n    2\n}".into()),
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        };
        let out = engine.code_apply(&req).expect("edit ok");
        assert!(
            out.contains("blast-radius") && out.contains("helper") && out.contains("src/b.rs"),
            "should surface callers of the edited symbol: {out}"
        );
    }

    #[test]
    fn fuzzy_recovery_suggests_near_miss_on_typo() {
        // SCRY-137: a mistyped identifier returns the nearest symbol instead of a
        // dead-end PATTERN_NO_MATCH — recover in one call.
        let engine = engine_with(&[("src/a.rs", "fn validate_token() {}\nfn parse_header() {}\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "validate_tokn".into(), // typo: missing 'e'
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("validate_token") && out.contains("fuzzy"),
            "should recover the near-miss symbol: {out}"
        );
        assert!(
            !out.contains("PATTERN_NO_MATCH"),
            "typo should recover, not dead-end: {out}"
        );
        // a genuinely-absent identifier (far from everything) still cleanly no-matches.
        let req2 = CodeRequest {
            queries: vec![Query {
                q: "zzqqxx_nothing".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(engine.code(&req2).contains("PATTERN_NO_MATCH"));
    }

    #[test]
    fn safe_delete_refuses_referenced_symbol() {
        // SCRY-134 (guiding master): deleting a symbol that still has references is
        // the dead-code/break-callers mistake; vyer refuses and names the sites.
        let mut engine = engine_with(&[
            ("src/a.rs", "pub fn helper() -> i32 {\n    1\n}\n"),
            ("src/b.rs", "fn use_it() -> i32 {\n    helper()\n}\n"),
        ]);
        engine.config.allow_writes = true;
        let del = |force: bool| ApplyRequest {
            edits: vec![Edit {
                locator: "src/a.rs#@delete:helper".into(),
                force,
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        };
        let err = engine.code_apply(&del(false)).unwrap_err();
        assert!(err.contains("refusing to delete `helper`"), "{err}");
        assert!(err.contains("src/b.rs"), "names the reference site: {err}");
        assert!(err.contains("force"), "offers the override: {err}");
        // force:true bypasses the guard (whatever happens next, it's not the refusal).
        let forced = engine.code_apply(&del(true));
        assert!(
            forced.is_ok() || !forced.unwrap_err().contains("refusing to delete"),
            "force:true must override the safe-delete guard"
        );
        // an UNREFERENCED symbol deletes without a refusal.
        let mut lone = engine_with(&[("src/c.rs", "fn dead() {}\n")]);
        lone.config.allow_writes = true;
        let out = lone.code_apply(&ApplyRequest {
            edits: vec![Edit {
                locator: "src/c.rs#@delete:dead".into(),
                ..Default::default()
            }],
            dry_run: true,
            undo: None,
            run: None,
        });
        assert!(
            out.is_ok() || !out.unwrap_err().contains("refusing to delete"),
            "unreferenced symbol must not be refused"
        );
    }

    #[test]
    fn scope_no_match_is_distinct_from_pattern_no_match() {
        let engine = engine_with(&[("src/auth.rs", "fn validate_token() {}\n")]);
        // scope resolves to ZERO files → SCOPE_NO_MATCH (the filter, not the pattern).
        let req = CodeRequest {
            queries: vec![Query {
                q: "validate_token".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                path_scope: vec!["does/not/exist/**".into()],
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("SCOPE_NO_MATCH"), "{out}");
        // pattern genuinely absent, scope fine → PATTERN_NO_MATCH.
        let req = CodeRequest {
            queries: vec![Query {
                q: "no_such_symbol_xyzzy".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("PATTERN_NO_MATCH"), "{out}");
    }

    #[test]
    fn plain_path_scope_matches_by_basename() {
        // SCRY-127: the reliability-floor regression — a bare filename in
        // path_scope must find the file (grep parity), not silently filter it out.
        let engine = engine_with(&[(
            "lib/game/config.dart",
            "class Config {\n  static const int stardustValue = 2;\n}\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "stardustValue".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                path_scope: vec!["config.dart".into()],
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        // The point of THIS test is scope leniency: the file is found, not
        // silently filtered out (which used to surface as SCOPE/PATTERN_NO_MATCH).
        assert!(
            out.contains("config.dart"),
            "lenient scope should locate it: {out}"
        );
        assert!(
            !out.contains("NO_MATCH"),
            "scope must not eat the search: {out}"
        );
    }

    #[test]
    fn dart_field_lookup_returns_field_not_god_class() {
        // SCRY-128 end-to-end: with the REAL tree-sitter parser (Engine::new),
        // a query for a class-level const lands on the FIELD's own small span,
        // not the enclosing 1242-line god-class. engine_with uses the heuristic
        // scanner (no fields), so this test indexes a real file from disk.
        let dir = std::env::temp_dir().join("vyer_dart_field_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/game")).unwrap();
        // a deliberately big class so a whole-class span would be obviously wrong.
        let mut body = String::from("class Config {\n");
        for i in 0..40 {
            body.push_str(&format!("  final int pad{i} = {i};\n"));
        }
        body.push_str("  static const int stardustValue = 2;\n");
        body.push_str("}\n");
        std::fs::write(dir.join("lib/game/config.dart"), body).unwrap();
        let engine = Engine::new(EngineConfig::new(dir.clone())).unwrap();
        let req = CodeRequest {
            queries: vec![Query {
                q: "stardustValue".into(),
                mode: "auto".into(),
                detail: "snippet".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.contains("stardustValue"),
            "field must be the returned span: {out}"
        );
        // the span must be the FIELD (one line), not the ~43-line class.
        assert!(
            out.contains("#stardustValue@"),
            "locator should anchor on the field, not the class: {out}"
        );
    }

    #[test]
    fn help_lists_every_mode_and_detail() {
        // SCRY-132: detail=help is the schema-as-truth surface; it must stay in sync
        // with the real param surface. Adding a mode/detail without listing it here
        // fails this test (drift guard).
        let engine = engine_with(&[("a.rs", "fn a() {}\n")]);
        let out = engine.code(&CodeRequest {
            queries: vec![Query {
                detail: "help".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        for m in [
            "auto",
            "lexical",
            "structural",
            "graph",
            "semantic",
            "ast",
            "diagnose",
        ] {
            assert!(out.contains(m), "help missing mode `{m}`: {out}");
        }
        for d in [
            "locate", "outline", "snippet", "full", "refs", "impact", "context", "count", "tree",
            "diff", "ast", "import", "help",
        ] {
            assert!(out.contains(d), "help missing detail `{d}`: {out}");
        }
        // the apply shape essentials an agent needs to form a valid first write.
        assert!(
            out.contains("new_body") && out.contains("locator") && out.contains("dry_run"),
            "help missing apply essentials: {out}"
        );
    }

    #[test]
    fn batch_query_fair_share_budget() {
        // SCRY-142: in a batch, a greedy query (many big matches) must not starve a
        // sibling — the sibling's top hit must still appear.
        let mut big = String::from("// greedy file\n");
        for i in 0..60 {
            big.push_str(&format!(
                "fn needle_{i}() {{\n    let x = needle_marker;\n}}\n"
            ));
        }
        let engine = engine_with(&[
            ("src/greedy.rs", &big),
            ("src/lonely.rs", "fn unique_sibling_symbol() {}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![
                Query {
                    q: "needle_marker".into(),
                    mode: "lexical".into(),
                    detail: "snippet".into(),
                    k: 60,
                    ..base_query()
                },
                Query {
                    q: "unique_sibling_symbol".into(),
                    mode: "lexical".into(),
                    detail: "locate".into(),
                    ..base_query()
                },
            ],
            budget_tokens: 1200, // tight, so a greedy q1 could starve q2 without fairness
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("unique_sibling_symbol"),
            "the sibling query's hit must survive a greedy batch-mate: {out}"
        );
    }

    #[test]
    fn snippet_windows_large_symbol_around_the_hit() {
        // SCRY-131: a "snippet" of a large symbol must be a WINDOW around the match,
        // not the whole 160-line body (which blew used=8000 truncated=true and
        // starved sibling queries in a batch).
        let mut body = String::from("fn big() {\n");
        for i in 0..80 {
            body.push_str(&format!("    let line{i} = {i};\n"));
        }
        body.push_str("    let NEEDLE_TOKEN = 1;\n");
        for i in 0..80 {
            body.push_str(&format!("    let tail{i} = {i};\n"));
        }
        body.push_str("}\n");
        let engine = engine_with(&[("src/big.rs", &body)]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "NEEDLE_TOKEN".into(),
                mode: "lexical".into(),
                detail: "snippet".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("NEEDLE_TOKEN"), "{out}");
        assert!(
            !out.contains("line0 = 0"),
            "snippet must not include the far-away symbol start: {out}"
        );
        assert!(
            !out.contains("tail79 = 79"),
            "snippet must not include the far-away symbol end: {out}"
        );
    }

    #[test]
    fn exact_symbol_suppresses_semantic_escalation() {
        // SCRY-131: an exact identifier hit must dominate; auto must NOT escalate to
        // semantic and dilute it with fuzzy neighbors.
        let engine = engine_with(&[
            ("src/a.rs", "fn slot_row() {}\n"),
            ("src/b.rs", "fn slot_row_helper() {}\nfn slot_menu() {}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "slot_row".into(),
                mode: "auto".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("slot_row"), "{out}");
        assert!(
            !out.contains("escalated to the semantic"),
            "exact symbol must not trigger semantic escalation: {out}"
        );
    }

    #[test]
    fn path_scope_holds_under_semantic_escalation() {
        // SCRY-131 regression: a NL query escalates to semantic+mention; the scope
        // filter must still confine results to the scoped file (no cross-file leak).
        let engine = engine_with(&[
            ("src/keep.rs", "fn compute_invoice_total() {}\n"),
            (
                "src/other.rs",
                "fn compute_invoice_total_other() {}\nfn invoice_helper() {}\n",
            ),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "invoice total computation".into(),
                mode: "auto".into(),
                detail: "locate".into(),
                path_scope: vec!["keep.rs".into()],
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            !out.contains("other.rs"),
            "path_scope leaked during semantic escalation: {out}"
        );
    }

    #[test]
    fn repo_map_dedups_per_file_symbols() {
        // SCRY-025: a struct and its impl both surface the name `Foo`; the
        // per-file symbol list in the repo-map must show it once, not twice.
        let engine = engine_with(&[(
            "a.rs",
            "struct Foo { x: u8 }\nimpl Foo {\n    fn make() -> Foo {\n        Foo { x: 0 }\n    }\n}\n",
        )]);
        let map = engine.repo_map(8000);
        let line = map
            .lines()
            .find(|l| l.contains("a.rs"))
            .expect("a.rs in map");
        let start = line.find('[').map(|i| i + 1).unwrap_or(0);
        let endb = line.rfind(']').unwrap_or(line.len());
        let bracket = &line[start..endb];
        let syms: Vec<&str> = bracket.split(", ").filter(|s| !s.is_empty()).collect();
        let uniq: HashSet<&str> = syms.iter().copied().collect();
        assert_eq!(
            syms.len(),
            uniq.len(),
            "duplicate symbols in repo-map line: {bracket}"
        );
    }

    #[test]
    fn repo_map_demotes_generated_below_source() {
        // SCRY-025c: `model.g.dart` is referenced MORE than `model.dart` (higher
        // raw PageRank), but as machine-generated it must still rank BELOW the
        // hand-written source after demotion — and be tagged `(gen)`.
        let engine = engine_with(&[
            ("model.dart", "class Mthing {}\n"),
            ("model.g.dart", "class Gthing {}\n"),
            ("use1.dart", "class U1 { Mthing a; Gthing b; }\n"),
            ("use2.dart", "class U2 { Gthing b; }\n"),
            ("use3.dart", "class U3 { Gthing b; }\n"),
        ]);
        let map = engine.repo_map(8000);
        assert!(
            map.contains("model.g.dart (gen)"),
            "generated file should be tagged (gen): {map}"
        );
        let lines: Vec<&str> = map.lines().collect();
        let idx = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle) && l.contains("rank="))
                .unwrap_or(usize::MAX)
        };
        assert!(
            idx("model.dart") < idx("model.g.dart"),
            "hand-written source must outrank its codegen despite higher raw centrality: {map}"
        );
        // sanity: the helper flags codegen, not plain sources.
        assert!(is_generated("a/b.g.dart") && is_generated("x_pb2.py"));
        assert!(!is_generated("src/engine.rs") && !is_generated("model.dart"));
    }

    #[test]
    fn generated_dir_files_are_tagged_codegen() {
        // SCRY-122: a file under a `generated/` directory is codegen even without a
        // codegen filename suffix (e.g. a `// @generated` TS type file).
        assert!(is_generated("generated/api_types.ts"));
        assert!(is_generated("src/generated/api_types.ts"));
        assert!(is_generated("app/__generated__/schema.py"));
        assert!(!is_generated("src/api_types.ts"));
        assert!(!is_generated("src/generator.rs"));
    }

    #[test]
    fn boolean_and_substring_recall() {
        // Both AND terms are substrings of larger identifiers; the file must match
        // (SCRY-038 on the boolean path), and a missing term must exclude it.
        let engine = engine_with(&[("src/a.rs", "let low_confidence = compute_threshold();\n")]);
        let q_hit = Query {
            q: String::new(),
            mode: "lexical".into(),
            detail: "locate".into(),
            all_of: vec!["confiden".into(), "threshold".into()],
            ..base_query()
        };
        let hit = CodeRequest {
            queries: vec![q_hit],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&hit).contains("src/a.rs"),
            "AND of two substrings lost recall"
        );

        let q_miss = Query {
            q: String::new(),
            mode: "lexical".into(),
            detail: "locate".into(),
            all_of: vec!["confiden".into(), "absentxyz".into()],
            ..base_query()
        };
        let miss = CodeRequest {
            queries: vec![q_miss],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&miss).contains("PATTERN_NO_MATCH"),
            "AND with an absent term must exclude the file"
        );
    }

    #[test]
    fn auto_escalates_to_semantic_when_lexical_structural_miss() {
        // The symbol is named `validate_token`; the query is natural language that
        // shares only the subword `token`. Lexical (multi-word literal) and
        // structural (name substring) both miss — only the semantic escalation
        // (Rule §5) recovers it.
        let engine = engine_with(&[(
            "src/auth.rs",
            "fn validate_token(tok: &str) -> bool { true }\n",
        )]);
        let nl = "check token valid";

        let lex = CodeRequest {
            queries: vec![Query {
                q: nl.into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&lex).contains("PATTERN_NO_MATCH"),
            "pure lexical should miss the natural-language query"
        );

        let auto = CodeRequest {
            queries: vec![Query {
                q: nl.into(),
                mode: "auto".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&auto);
        assert!(
            out.contains("src/auth.rs"),
            "auto should escalate to semantic and surface the symbol: {out}"
        );
    }

    #[test]
    fn path_scope_supports_exclusions() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        // positive only
        assert!(path_in_scope("src/a.rs", &s(&["src/**"])));
        assert!(!path_in_scope("docs/a.md", &s(&["src/**"])));
        // exclusion wins over a matching positive
        assert!(!path_in_scope(
            "src/tests/x.rs",
            &s(&["src/**", "!**/tests/**"])
        ));
        assert!(path_in_scope(
            "src/auth/x.rs",
            &s(&["src/**", "!**/tests/**"])
        ));
        // exclusion-only: everything not excluded is in scope
        assert!(path_in_scope("src/a.rs", &s(&["!**/vendor/**"])));
        assert!(!path_in_scope("x/vendor/y.rs", &s(&["!**/vendor/**"])));
        // empty scope = whole repo
        assert!(path_in_scope("anything.rs", &[]));

        // SCRY-127: a wildcard-free entry is a lenient path-component match (the
        // grep-equivalence floor), not a strict full-path equality.
        assert!(path_in_scope("lib/game/config.dart", &s(&["config.dart"]))); // basename
        assert!(path_in_scope(
            "lib/game/config.dart",
            &s(&["game/config.dart"])
        )); // subpath
        assert!(path_in_scope(
            "lib/game/config.dart",
            &s(&["lib/game/config.dart"])
        )); // exact
        assert!(path_in_scope("lib/game/config.dart", &s(&["game"]))); // interior dir
        assert!(path_in_scope("lib/game/config.dart", &s(&["lib"]))); // top dir
        assert!(!path_in_scope("lib/game/widget.dart", &s(&["config.dart"]))); // no false hit
        assert!(!path_in_scope("lib/gamepad/config.dart", &s(&["game"]))); // boundary, not substring
                                                                           // lenient exclusion too: `!config.dart` drops the named file.
        assert!(!path_in_scope(
            "lib/game/config.dart",
            &s(&["!config.dart"])
        ));

        // integration: a real search with an exclusion drops the excluded file.
        let engine = engine_with(&[
            ("src/auth.rs", "fn validate_token() {}\n"),
            ("src/tests/auth_test.rs", "fn validate_token() {}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "validate_token".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                path_scope: s(&["src/**", "!**/tests/**"]),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("src/auth.rs"), "kept file missing: {out}");
        assert!(
            !out.contains("tests/auth_test.rs"),
            "excluded file leaked into results: {out}"
        );
    }

    #[test]
    fn subtree_outline_maps_scoped_files() {
        let engine = engine_with(&[
            ("src/auth.rs", "fn validate_token() {}\nfn refresh() {}\n"),
            ("src/db.rs", "fn connect() {}\n"),
            ("src/tests/auth_test.rs", "fn t() {}\n"),
        ]);
        let req = CodeRequest {
            queries: vec![Query {
                q: String::new(),
                mode: "auto".into(),
                detail: "outline".into(),
                path_scope: vec!["src/**".into(), "!**/tests/**".into()],
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("validate_token"),
            "auth outline missing: {out}"
        );
        assert!(out.contains("connect"), "db outline missing: {out}");
        assert!(
            !out.contains("auth_test.rs"),
            "excluded test dir leaked into the subtree outline: {out}"
        );
    }

    #[test]
    fn no_match_hint_is_tailored() {
        let engine = engine_with(&[("src/a.rs", "fn alpha() {}\n")]);
        let mk = |q: Query| CodeRequest {
            queries: vec![q],
            budget_tokens: 8000,
            exclude_seen: false,
        };

        // multi-word miss → points at the semantic/ast escalation
        let mw = engine.code(&mk(Query {
            q: "no such concept here".into(),
            mode: "auto".into(),
            detail: "locate".into(),
            ..base_query()
        }));
        assert!(mw.contains("PATTERN_NO_MATCH"));
        assert!(
            mw.contains("semantic") || mw.contains("mode=ast"),
            "multi-word hint not tailored: {mw}"
        );

        // lang-filtered miss → names the lang filter
        let ln = engine.code(&mk(Query {
            q: "zzzmissing".into(),
            mode: "lexical".into(),
            detail: "locate".into(),
            lang: Some("python".into()),
            ..base_query()
        }));
        assert!(ln.contains("lang"), "lang hint not tailored: {ln}");

        // exclusion miss → calls out the over-filtering exclusion
        let ex = engine.code(&mk(Query {
            q: "alpha".into(),
            mode: "lexical".into(),
            detail: "locate".into(),
            path_scope: vec!["!**/*.rs".into()],
            ..base_query()
        }));
        assert!(ex.contains("PATTERN_NO_MATCH"));
        assert!(
            ex.contains("exclusion"),
            "exclusion hint not tailored: {ex}"
        );
    }

    #[test]
    fn lang_filter_accepts_multiple_languages() {
        let engine = engine_with(&[
            ("a.ts", "function foo() {}\n"),
            ("b.js", "function foo() {}\n"),
            ("c.py", "def foo(): pass\n"),
        ]);
        let mk = |lang: Option<String>| CodeRequest {
            queries: vec![Query {
                q: "foo".into(),
                mode: "lexical".into(),
                detail: "locate".into(),
                lang,
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        // single language
        let ts = engine.code(&mk(Some("ts".into())));
        assert!(ts.contains("a.ts"), "ts file missing: {ts}");
        assert!(
            !ts.contains("b.js") && !ts.contains("c.py"),
            "ts filter leaked: {ts}"
        );
        // comma-separated list (polyglot)
        let both = engine.code(&mk(Some("ts,js".into())));
        assert!(
            both.contains("a.ts") && both.contains("b.js"),
            "ts,js should include both: {both}"
        );
        assert!(!both.contains("c.py"), "py should stay excluded: {both}");
    }

    #[test]
    fn lang_filter_supports_text_formats_and_warns_on_unknown() {
        // SCRY-124 (#4): yaml/json/etc are extension-filterable; a truly unknown lang
        // warns instead of silently returning nothing.
        let engine = engine_with(&[
            ("config/app.yaml", "channels:\n  - email\n"),
            ("src/a.rs", "fn channels() {}\n"),
        ]);
        let y = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "channels".into(),
                lang: Some("yaml".into()),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            y.contains("app.yaml"),
            "yaml filter should find the yaml file: {y}"
        );
        assert!(!y.contains("a.rs"), "yaml filter must exclude rust: {y}");

        let u = engine.code(&CodeRequest {
            queries: vec![Query {
                q: "channels".into(),
                lang: Some("klingon".into()),
                mode: "lexical".into(),
                detail: "locate".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        });
        assert!(
            u.contains("unknown lang `klingon`"),
            "unknown lang must warn: {u}"
        );
    }

    #[test]
    fn context_callees_exclude_comments_and_params() {
        // `text` is a real symbol (so it's in by_name) but in `target` it is only
        // used as a value, never CALLED; `helper` is genuinely called. The old
        // "any matching identifier" logic listed both; the call-site logic must
        // list only `helper`.
        let engine = engine_with(&[(
            "src/a.rs",
            "fn text() {}\n\
             fn helper(x: u8) -> u8 { x }\n\
             fn target(p: u8) -> u8 {\n\
             // text helper mentioned in a comment, not called\n\
             let q = text;\n\
             helper(p)\n\
             }\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let calls_line = out.lines().find(|l| l.starts_with("[calls]")).unwrap_or("");
        assert!(
            calls_line.contains("helper"),
            "genuine callee missing from [calls]: {calls_line}"
        );
        assert!(
            !calls_line.contains("text"),
            "param/comment noise `text` leaked into [calls]: {calls_line}"
        );
    }

    #[test]
    fn context_callees_survive_rust_lifetimes() {
        // A lifetimed Rust fn: a naive "scan to the next quote" treats `'a` as a
        // string open and swallows the call between ticks. The lang-aware scan
        // (in Rust `'` is a lifetime, not a string) must still see `helper`.
        let engine = engine_with(&[(
            "src/a.rs",
            "fn helper(x: u8) -> u8 { x }\n\
             fn target<'a, 'b>(p: &'a u8, q: &'b u8) -> u8 {\n\
             helper(*p)\n\
             }\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let calls_line = out.lines().find(|l| l.starts_with("[calls]")).unwrap_or("");
        assert!(
            calls_line.contains("helper"),
            "a Rust lifetime swallowed the callee: {calls_line}"
        );
    }

    #[test]
    fn context_callees_skip_single_quoted_strings_in_python() {
        // In Python `'` DOES delimit a string, so `'ghost('` must not be read as a
        // call. (The lang-aware `'` handling must cut both ways: lifetime in Rust,
        // string in Python.)
        let engine = engine_with(&[(
            "a.py",
            "def helper():\n    return 1\n\ndef ghost():\n    return 2\n\ndef target():\n    s = 'ghost('\n    helper()\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        let calls_line = out.lines().find(|l| l.starts_with("[calls]")).unwrap_or("");
        assert!(
            calls_line.contains("helper"),
            "real callee missing: {calls_line}"
        );
        assert!(
            !calls_line.contains("ghost"),
            "a single-quoted Python string was read as a call: {calls_line}"
        );
    }

    #[test]
    fn context_callees_include_recursive_self_call() {
        // SCRY-052: a recursive function calls ITSELF — that self-call is a real
        // callee (the `!= target` filter used to wrongly drop it).
        let engine = engine_with(&[(
            "r.rs",
            "fn fact(n: u64) -> u64 {\n    if n <= 1 { 1 } else { n * fact(n - 1) }\n}\nfn helper() {}\n",
        )]);
        let calls_for = |sym: &str, eng: &Engine| -> String {
            let req = CodeRequest {
                queries: vec![Query {
                    q: sym.into(),
                    mode: "auto".into(),
                    detail: "context".into(),
                    ..base_query()
                }],
                budget_tokens: 8000,
                exclude_seen: false,
            };
            let out = eng.code(&req);
            out.lines()
                .find(|l| l.starts_with("[calls]"))
                .unwrap_or("")
                .to_string()
        };
        assert!(
            calls_for("fact", &engine).contains("fact"),
            "recursive self-call missing from [calls]"
        );
        // a non-recursive symbol must NOT list itself (only its declaration).
        assert!(
            !calls_for("helper", &engine).contains("helper"),
            "non-recursive fn wrongly self-listed"
        );
    }

    #[test]
    fn context_notes_ambiguous_symbol() {
        // SCRY-053: with two `dup` defs, context analyzes the first but tells the
        // agent there are 2 and lists the other to disambiguate.
        let engine = engine_with(&[("a.rs", "fn dup() { 1 }\n"), ("b.rs", "fn dup() { 2 }\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "dup".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("2 def(s)"), "should count both defs: {out}");
        assert!(
            out.contains("Disambiguate") && out.contains("symbols named"),
            "should note the ambiguity and list the other def: {out}"
        );
    }

    #[test]
    fn scan_idents_edge_cases() {
        // Rust mode (sq_is_string=false): direct calls found, char literals skipped.
        let rust = "let c = 'x'; foo(); bar(); '\\n'; baz()";
        let calls = scan_idents(rust, true, vyer_incr::Lang::Rust);
        assert!(
            calls.contains("foo") && calls.contains("bar") && calls.contains("baz"),
            "direct calls missed: {calls:?}"
        );
        assert!(
            !calls.contains("x") && !calls.contains("n"),
            "char literal leaked: {calls:?}"
        );

        // Python mode (sq_is_string=true): single-quoted strings skipped.
        let py = "f(); s = 'ghost('; g()";
        let pcalls = scan_idents(py, true, vyer_incr::Lang::Python);
        assert!(
            pcalls.contains("f") && pcalls.contains("g"),
            "py calls missed: {pcalls:?}"
        );
        assert!(
            !pcalls.contains("ghost"),
            "py string leaked as call: {pcalls:?}"
        );

        // Turbofish call sites still register the function (graph fidelity).
        let tf = scan_idents(
            "parse::<u32>(); collect::<Vec<u8>>()",
            true,
            vyer_incr::Lang::Rust,
        );
        assert!(
            tf.contains("parse") && tf.contains("collect"),
            "turbofish call missed: {tf:?}"
        );
        assert!(
            !tf.contains("Vec") && !tf.contains("u8"),
            "generic arg counted as call: {tf:?}"
        );

        // Unterminated string / block comment / trailing `(` must not panic.
        let _ = scan_idents("foo(\"unterminated", true, vyer_incr::Lang::Rust);
        let _ = scan_idents("foo(); /* unterminated", true, vyer_incr::Lang::Rust);
        let _ = scan_idents("end_call(", true, vyer_incr::Lang::Rust);

        // Reference mode (calls_only=false): code idents kept, comment words not.
        let refs = scan_idents("alpha; // beta\n gamma()", false, vyer_incr::Lang::Rust);
        assert!(
            refs.contains("alpha") && refs.contains("gamma"),
            "refs missed: {refs:?}"
        );
        assert!(
            !refs.contains("beta"),
            "comment word leaked into refs: {refs:?}"
        );

        // SCRY-110: a name mentioned only inside a BACKTICK string (JS/TS template,
        // Go raw string) is NOT a reference (backtick_string=true).
        let bt = scan_idents(
            "real(); const m = `call ghost now`;",
            false,
            vyer_incr::Lang::JavaScript,
        );
        assert!(bt.contains("real"), "code call missed: {bt:?}");
        assert!(
            !bt.contains("ghost"),
            "backtick-string mention leaked as a reference: {bt:?}"
        );
    }

    #[test]
    fn context_callers_exclude_comment_only_mentions() {
        let engine = engine_with(&[(
            "src/a.rs",
            "fn target() {}\n\
             fn real_caller() { target() }\n\
             fn fake_caller() {\n\
             // target is only named in this comment\n\
             let s = \"target\";\n\
             }\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "auto".into(),
                detail: "context".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("real_caller"), "real caller missing: {out}");
        assert!(
            !out.contains("fake_caller"),
            "a comment/string-only mention became a false caller: {out}"
        );
    }

    #[test]
    fn impact_excludes_comment_only_referrers() {
        let engine = engine_with(&[(
            "src/a.rs",
            "fn target() {}\n\
             fn real_ref() { target() }\n\
             fn fake_ref() {\n\
             // calls target eventually\n\
             let s = \"target\";\n\
             }\n",
        )]);
        let req = CodeRequest {
            queries: vec![Query {
                q: "target".into(),
                mode: "auto".into(),
                detail: "impact".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(out.contains("real_ref"), "real referrer missing: {out}");
        // real_ref is a depth-1 (direct) referrer — surfaced as the actionable count.
        assert!(
            out.contains("1 direct"),
            "direct-referrer count missing: {out}"
        );
        assert!(
            !out.contains("fake_ref"),
            "comment/string-only mention became a false referrer in the blast radius: {out}"
        );
    }

    #[test]
    fn count_supports_boolean_queries() {
        let engine = engine_with(&[
            ("a.rs", "let x = alpha + beta;\nlet y = alpha;\n"),
            ("b.rs", "let z = beta;\n"),
        ]);
        // all_of [alpha, beta] → only line 1 of a.rs matches both.
        let req = CodeRequest {
            queries: vec![Query {
                q: String::new(),
                mode: "lexical".into(),
                detail: "count".into(),
                all_of: vec!["alpha".into(), "beta".into()],
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("1 matching lines across 1 files"),
            "boolean count wrong: {out}"
        );
        assert!(
            out.contains("a.rs") && !out.contains("b.rs"),
            "boolean count file set wrong: {out}"
        );
    }

    #[test]
    fn dump_ast_lists_node_kinds() {
        let engine = engine_with(&[("a.py", "class W:\n    def m(self):\n        return 1\n")]);
        let req = CodeRequest {
            queries: vec![Query {
                path: Some("a.py".into()),
                detail: "ast".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("class_definition"),
            "no class node kind: {out}"
        );
        assert!(
            out.contains("function_definition"),
            "no function node kind: {out}"
        );
        // Field names are emitted so field-qualified mode=ast queries are author-able.
        assert!(
            out.contains("name: (identifier)"),
            "no field-qualified node: {out}"
        );

        // `lines` filter prunes out-of-range subtrees (the L2-3 method) while
        // keeping the enclosing class that overlaps L1.
        let scoped = CodeRequest {
            queries: vec![Query {
                path: Some("a.py".into()),
                detail: "ast".into(),
                lines: Some("1".into()),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let s = engine.code(&scoped);
        assert!(
            s.contains("class_definition"),
            "in-range class missing: {s}"
        );
        assert!(
            !s.contains("function_definition"),
            "out-of-range method should be pruned by lines=1: {s}"
        );

        // `q`=symbol name dumps just that symbol's AST (no line math).
        let bysym = CodeRequest {
            queries: vec![Query {
                path: Some("a.py".into()),
                q: "m".into(),
                detail: "ast".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let sym_out = engine.code(&bysym);
        assert!(
            sym_out.contains("function_definition"),
            "symbol-scoped dump should include the method: {sym_out}"
        );
        // A missing symbol is an actionable message (lists the file's symbols).
        let missing = CodeRequest {
            queries: vec![Query {
                path: Some("a.py".into()),
                q: "nope".into(),
                detail: "ast".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&missing).contains("not found"),
            "missing symbol should be reported"
        );

        // detail=ast without a path is an actionable message, not a crash.
        let nopath = CodeRequest {
            queries: vec![Query {
                detail: "ast".into(),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        assert!(
            engine.code(&nopath).contains("needs a `path`"),
            "detail=ast should require a path"
        );
    }

    #[test]
    fn mode_ast_accepts_a_path_scope() {
        let engine = engine_with(&[
            ("a.py", "class W:\n    def m(self):\n        return 1\n"),
            ("b.py", "class Other:\n    pass\n"),
        ]);
        // path scopes the structural query to a single file.
        let req = CodeRequest {
            queries: vec![Query {
                q: "(class_definition name: (identifier) @c)".into(),
                mode: "ast".into(),
                detail: "locate".into(),
                path: Some("a.py".into()),
                ..base_query()
            }],
            budget_tokens: 8000,
            exclude_seen: false,
        };
        let out = engine.code(&req);
        assert!(
            out.contains("a.py"),
            "ast query should match the scoped file: {out}"
        );
        assert!(
            !out.contains("b.py"),
            "ast query must not touch other files: {out}"
        );
    }

    #[test]
    fn semantic_cache_invalidates_after_write() {
        // Rule §2: the new semantic-index cache must NEVER serve a stale corpus.
        // After a write bumps the revision, an escalation must see the new symbol.
        let engine = engine_with(&[("a.rs", "fn alpha_token() {}\n")]);
        {
            let db = engine.db.lock().unwrap();
            let q = Query {
                q: "token".into(),
                mode: "semantic".into(),
                ..base_query()
            };
            let files = engine.scoped_files(&db, &q);
            let mut c: HashMap<String, Cand> = HashMap::new();
            let ids = engine.semantic_ids(&db, &q, &files, &mut c);
            assert!(
                ids.iter().any(|id| id.contains("alpha_token")),
                "first escalation should find alpha_token: {ids:?}"
            );
        }
        // a write adds a new symbol — the revision changes, the cache must rebuild.
        engine
            .db
            .lock()
            .unwrap()
            .set_text("b.rs", "fn beta_token() {}\n");
        {
            let db = engine.db.lock().unwrap();
            let q = Query {
                q: "beta".into(),
                mode: "semantic".into(),
                ..base_query()
            };
            let files = engine.scoped_files(&db, &q);
            let mut c: HashMap<String, Cand> = HashMap::new();
            let ids = engine.semantic_ids(&db, &q, &files, &mut c);
            assert!(
                ids.iter().any(|id| id.contains("beta_token")),
                "STALE semantic cache: beta_token not seen after the write: {ids:?}"
            );
        }
    }
}
