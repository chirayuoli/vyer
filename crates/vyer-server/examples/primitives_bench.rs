//! Empirical proof that Vyer's warm primitives beat the coreutils they replace.
//!
//! Builds the engine ONCE over a repo (the resident-daemon steady state), then
//! times the new primitives — line-range read (sed/head/tail), `detail=count`
//! (grep -c), and `detail=tree` (find) — against the real coreutils invoked as
//! subprocesses (what an agent shelling out actually pays: fork/exec + cold
//! open/read + full scan, every call).
//!
//! HONEST FRAMING (CLAUDE.md §7 / todo #10): this measures the WARM core. A
//! one-shot `vyer` CLI call must first index the repo and would lose to grep on
//! a tiny tree — the advantage is the resident core, not a faster regex. We
//! print Vyer's one-time index cost up front so the comparison is not cherry-
//! picked.
//!
//! Run: cargo run -p vyer-server --example primitives_bench --release -- [root]

use std::process::Command;
use std::time::Instant;

use vyer_server::engine::{CodeRequest, Engine, EngineConfig, Query};

fn base() -> Query {
    Query {
        q: String::new(),
        path: None,
        mode: "lexical".into(),
        detail: "snippet".into(),
        path_scope: vec![],
        lang: None,
        lines: None,
        all_of: vec![],
        any_of: vec![],
        none_of: vec![],
        k: 8,
    }
}

fn req(q: Query) -> CodeRequest {
    CodeRequest {
        queries: vec![q],
        budget_tokens: 8000,
        exclude_seen: false,
    }
}

/// p50 wall time (µs) of `f` over `iters` runs, after a warmup.
fn p50_us<F: FnMut()>(iters: usize, mut f: F) -> u64 {
    f(); // warmup
    let mut s = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f();
        s.push(t.elapsed().as_micros() as u64);
    }
    s.sort_unstable();
    s[s.len() / 2]
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let t0 = Instant::now();
    let engine = Engine::new(EngineConfig::new(root.clone().into())).expect("index");
    let files = engine.indexed_files();
    println!(
        "Vyer indexed {} files from {root} in {} ms (paid ONCE; the daemon stays warm)\n",
        files.len(),
        t0.elapsed().as_millis()
    );

    // Pick the largest indexed file for the read benchmarks.
    let target = files
        .iter()
        .max_by_key(|f| {
            std::fs::metadata(format!("{root}/{f}"))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .cloned()
        .expect("at least one file");
    let abs = format!("{root}/{target}");
    let iters = 300usize;

    println!(
        "{:<28} {:>12} {:>12} {:>9}",
        "primitive", "vyer warm", "coreutil", "speedup"
    );
    println!("{}", "-".repeat(64));

    // 1) line-range read (mid-file 100 lines) vs `sed -n`
    let vyer_sed = p50_us(iters, || {
        let q = Query {
            path: Some(target.clone()),
            lines: Some("200-300".into()),
            detail: "full".into(),
            ..base()
        };
        let _ = engine.code(&req(q));
    });
    let sed = p50_us(iters / 3, || {
        let _ = Command::new("sed").args(["-n", "200,300p", &abs]).output();
    });
    row("read lines 200-300 (sed -n)", vyer_sed, sed);

    // 2) tail 20 lines vs `tail -20`
    let vyer_tail = p50_us(iters, || {
        let q = Query {
            path: Some(target.clone()),
            lines: Some("~20".into()),
            detail: "full".into(),
            ..base()
        };
        let _ = engine.code(&req(q));
    });
    let tail = p50_us(iters / 3, || {
        let _ = Command::new("tail").args(["-20", &abs]).output();
    });
    row("tail 20 (tail -20)", vyer_tail, tail);

    // 3) count a common token vs `grep -rc`
    let token = "fn";
    let vyer_count = p50_us(iters, || {
        let q = Query {
            q: token.into(),
            detail: "count".into(),
            ..base()
        };
        let _ = engine.code(&req(q));
    });
    let grep = p50_us(iters / 3, || {
        let _ = Command::new("grep").args(["-rc", token, &root]).output();
    });
    row(&format!("count \"{token}\" (grep -rc)"), vyer_count, grep);

    // 4) tree listing vs `find -type f`
    let vyer_tree = p50_us(iters, || {
        let q = Query {
            detail: "tree".into(),
            ..base()
        };
        let _ = engine.code(&req(q));
    });
    let find = p50_us(iters / 3, || {
        let _ = Command::new("find").args([&root, "-type", "f"]).output();
    });
    row("tree (find -type f)", vyer_tree, find);

    println!("\nWarm Vyer serves these from RAM (no fork/exec, no disk re-read);");
    println!("the coreutil pays process spawn + cold I/O + full scan on every call.");
}

fn row(name: &str, vyer_us: u64, util_us: u64) {
    let speedup = util_us as f64 / vyer_us.max(1) as f64;
    println!(
        "{:<28} {:>9.3} ms {:>9.3} ms {:>8.1}x",
        name,
        vyer_us as f64 / 1000.0,
        util_us as f64 / 1000.0,
        speedup
    );
}
