//! Warm-query latency check against the SLOs (CLAUDE.md §7). Builds the engine
//! once over a repo (default `.`), then times many queries against the warm
//! core — which is what a resident daemon actually serves (the CLI's per-call
//! cold index+parse is not the steady state). Run:
//!   cargo run -p vyer-server --example warm_bench --release -- [root]

use std::time::Instant;

use vyer_server::engine::{CodeRequest, Engine, EngineConfig, Query};

fn q(s: &str, mode: &str, detail: &str) -> CodeRequest {
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
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let build_start = Instant::now();
    let engine = Engine::new(EngineConfig::new(root.clone().into())).expect("index");
    let files = engine.indexed_files().len();
    println!(
        "indexed {files} files from {root} in {} ms (cold; daemon pays this once)",
        build_start.elapsed().as_millis()
    );

    let queries = [
        ("validate_write", "auto", "snippet"),
        ("pagerank", "structural", "outline"),
        ("Engine", "lexical", "locate"),
        ("set_text", "auto", "snippet"),
        ("fn parse", "lexical", "snippet"),
        ("make_id", "graph", "refs"),
        ("Engine", "auto", "context"),
        ("make_id", "graph", "impact"),
    ];

    // warmup
    for (s, m, d) in &queries {
        let _ = engine.code(&q(s, m, d));
    }

    let mut samples_us = Vec::new();
    let iters = 200usize;
    for i in 0..iters {
        let (s, m, d) = queries[i % queries.len()];
        let t = Instant::now();
        let _ = engine.code(&q(s, m, d));
        samples_us.push(t.elapsed().as_micros() as u64);
    }
    samples_us.sort_unstable();
    let p = |q: f64| samples_us[((samples_us.len() as f64 * q) as usize).min(samples_us.len() - 1)];
    println!(
        "warm query latency over {iters} runs: p50={:.2} ms  p95={:.2} ms  max={:.2} ms",
        p(0.50) as f64 / 1000.0,
        p(0.95) as f64 / 1000.0,
        *samples_us.last().unwrap() as f64 / 1000.0,
    );
    println!("SLO targets: locate/outline p50<30ms p95<120ms; snippet p50<50ms");
}
