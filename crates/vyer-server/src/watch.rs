//! A best-effort filesystem watcher that keeps the warm core fresh for edits
//! made *outside* `code_apply` (an external editor, `git checkout`, a formatter,
//! or a scaffolder like `flutter create` / `npm init` that drops many files at once).
//!
//! Freshness through `code_apply` is already synchronous and exact (Rule §2); this is
//! the complementary path for out-of-band changes. It is intentionally degradable: if
//! the watcher can't start, the server still runs (queries reflect the last index
//! until the next `code_apply` or restart).
//!
//! Two layers, because a single FS event stream is not reliable on its own:
//!   1. **Per-path** — each create/modify/remove event re-indexes that file
//!      immediately, so a one-off external edit is query-ready right away.
//!   2. **Debounced full re-scan** — any burst of events also schedules one
//!      `reindex_all` after activity settles. This is what makes a tool that creates
//!      hundreds of files into brand-new subdirectories (`flutter create`) reliable:
//!      even if the OS drops events or doesn't deliver new-subdir creates, the
//!      re-scan walks the tree fresh and picks up everything. (SCRY-115)

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::engine::Engine;

/// Coalesce a storm of events into a single re-scan: once activity starts, wait until
/// it's been quiet for this long, then reconcile. Long enough to let a scaffolder
/// finish, short enough to feel instant to the agent's next query.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(400);

/// Start watching `root`. The returned watcher must be kept alive for the duration of
/// the server (dropping it stops watching and ends the re-scan thread). Errors are
/// returned so the caller can log-and-continue rather than fail.
pub fn start(engine: Arc<Engine>) -> notify::Result<RecommendedWatcher> {
    let root = engine.root().to_path_buf();

    // The debounced re-scan worker. It blocks until the watcher signals activity, then
    // drains the channel until things go quiet (`RESCAN_DEBOUNCE`) and does ONE full
    // reconcile. When the watcher (and its `tx`) is dropped, `recv` errors and the
    // thread exits.
    let (tx, rx) = mpsc::channel::<()>();
    let rescan_engine = engine.clone();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            while rx.recv_timeout(RESCAN_DEBOUNCE).is_ok() {}
            let _ = rescan_engine.reindex_all();
        }
    });

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };
        // Only react to content-changing events; ignore access/metadata noise.
        if !(event.kind.is_create() || event.kind.is_modify() || event.kind.is_remove()) {
            return;
        }
        // Layer 1: re-index each named path immediately (cheap; a no-op when content is
        // unchanged, so a storm of spurious events costs nothing).
        for path in event.paths {
            if let Some(rel) = engine.rel_of(&path) {
                engine.reindex_path(&rel);
            }
        }
        // Layer 2: signal a debounced full re-scan to catch anything the per-path pass
        // missed (dropped events, files in newly-created subdirectories, bursts).
        let _ = tx.send(());
    })?;
    watcher.watch(Path::new(&root), RecursiveMode::Recursive)?;
    Ok(watcher)
}
