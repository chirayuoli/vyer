//! A best-effort filesystem watcher that keeps the warm core fresh for edits
//! made *outside* `code_apply` (an external editor, `git checkout`, a formatter).
//!
//! Freshness through `code_apply` is already synchronous and exact (Rule §2);
//! this is the complementary path for out-of-band changes. It is intentionally
//! degradable: if the watcher can't start, the server still runs (queries just
//! reflect the last index until the next `code_apply` or restart).

use std::path::Path;
use std::sync::Arc;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::engine::Engine;

/// Start watching `root`. The returned watcher must be kept alive for the
/// duration of the server (dropping it stops watching). Errors are returned so
/// the caller can log-and-continue rather than fail.
pub fn start(engine: Arc<Engine>) -> notify::Result<RecommendedWatcher> {
    let root = engine.root().to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };
        // Only react to content-changing events; ignore access/metadata noise.
        if !(event.kind.is_create() || event.kind.is_modify() || event.kind.is_remove()) {
            return;
        }
        for path in event.paths {
            if let Some(rel) = engine.rel_of(&path) {
                // reindex_path is a no-op when content is unchanged, so a storm
                // of spurious events is cheap.
                engine.reindex_path(&rel);
            }
        }
    })?;
    watcher.watch(Path::new(&root), RecursiveMode::Recursive)?;
    Ok(watcher)
}
