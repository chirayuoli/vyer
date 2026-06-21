//! Measures the POST-EDIT query cost (CLAUDE.md §7: "incremental re-index /
//! edited file < 50ms to query-ready"). The token/semantic indexes are keyed by
//! the global revision, so any edit triggers a FULL rebuild on the next query.
//! This bench quantifies that rebuild against repo size to decide whether an
//! incremental index is warranted. Run:
//!   cargo run -p vyer-server --example reindex_bench --release -- [n_files]

use std::time::Instant;

use vyer_server::engine::{CodeRequest, Engine, EngineConfig, Query};

fn q(s: &str) -> CodeRequest {
    qm(s, "lexical")
}

fn qm(s: &str, mode: &str) -> CodeRequest {
    qmd(s, mode, "snippet")
}

fn qmd(s: &str, mode: &str, detail: &str) -> CodeRequest {
    CodeRequest {
        queries: vec![Query {
            q: s.into(),
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

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    let dir = std::env::temp_dir().join(format!("vyer_reindex_bench_{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..n {
        std::fs::write(
            dir.join(format!("f{i}.rs")),
            format!("fn func_{i}() -> i32 {{ {i} }}\npub struct S{i};\n"),
        )
        .unwrap();
    }

    let cold = Instant::now();
    let engine = Engine::new(EngineConfig::new(dir.clone())).expect("index");
    println!(
        "indexed {} files in {:.0}ms (daemon cold start)",
        engine.indexed_files().len(),
        cold.elapsed().as_secs_f64() * 1000.0
    );

    // FIRST query builds the token index from empty (the full O(repo) build).
    // Use the HIGHEST index as a near-unique token (`func_4999` isn't a substring
    // of any longer name) so the number reflects a specific query, not a broad
    // substring match across ~1000 files.
    let uniq = format!("func_{}", n - 1);
    let t = Instant::now();
    let _ = engine.code(&q(&uniq));
    let cold_build = t.elapsed();
    // steady-state warm query (index already current).
    let t = Instant::now();
    let _ = engine.code(&q(&uniq));
    let warm = t.elapsed();

    // change one file on disk + reindex → revision bump → next query rebuilds.
    std::fs::write(dir.join("f0.rs"), "fn func_0_changed() -> i32 { 999 }\n").unwrap();
    engine.reindex_path("f0.rs");
    // a near-unique token again (func_{n-2}), so the delta vs `warm` is the index
    // refresh cost, not a broad substring match.
    let uniq2 = format!("func_{}", n - 2);
    let t = Instant::now();
    let _ = engine.code(&q(&uniq2));
    let post_edit = t.elapsed();

    // other warm modes at scale — confirm none hide an O(repo) per-query cost.
    let _ = engine.code(&qm(&uniq, "structural"));
    let t = Instant::now();
    let _ = engine.code(&qm(&uniq, "structural"));
    let struct_warm = t.elapsed();
    let _ = engine.code(&qm(&uniq, "graph"));
    let t = Instant::now();
    let _ = engine.code(&qm(&uniq, "graph"));
    let graph_warm = t.elapsed();
    let _ = engine.code(&qmd(&uniq, "graph", "context"));
    let t = Instant::now();
    let _ = engine.code(&qmd(&uniq, "graph", "context"));
    let ctx_warm = t.elapsed();
    let _ = engine.code(&qmd(&uniq, "graph", "impact"));
    let t = Instant::now();
    let _ = engine.code(&qmd(&uniq, "graph", "impact"));
    let impact_warm = t.elapsed();
    println!(
        "n={n}  structural={:.2}ms  refs={:.2}ms  context={:.2}ms  impact={:.2}ms",
        struct_warm.as_secs_f64() * 1000.0,
        graph_warm.as_secs_f64() * 1000.0,
        ctx_warm.as_secs_f64() * 1000.0,
        impact_warm.as_secs_f64() * 1000.0
    );

    // SEMANTIC index: same full-rebuild-on-revision pattern. Measure cold build vs
    // post-edit cost to decide whether it ALSO needs the incremental treatment.
    let _ = engine.code(&qm("function value", "semantic")); // cold build
    let t = Instant::now();
    let _ = engine.code(&qm("structure thing", "semantic")); // warm
    let sem_warm = t.elapsed();
    std::fs::write(dir.join("f1.rs"), "fn f1_changed() {}\n").unwrap();
    engine.reindex_path("f1.rs");
    let t = Instant::now();
    let _ = engine.code(&qm("another concept", "semantic")); // post-edit (rebuild)
    let sem_post = t.elapsed();

    println!(
        "n={n}  token: cold={:.1}ms warm={:.2}ms post-edit={:.2}ms [{}]   semantic: warm={:.2}ms post-edit={:.1}ms [{}]",
        cold_build.as_secs_f64() * 1000.0,
        warm.as_secs_f64() * 1000.0,
        post_edit.as_secs_f64() * 1000.0,
        if post_edit.as_millis() < 50 { "OK" } else { "VIOLATED" },
        sem_warm.as_secs_f64() * 1000.0,
        sem_post.as_secs_f64() * 1000.0,
        if sem_post.as_millis() < 50 { "OK" } else { "VIOLATED" }
    );
    let _ = std::fs::remove_dir_all(&dir);
}
