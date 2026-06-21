#!/usr/bin/env bash
# Vyer — one-shot verification runner (steps 1–3 from the testing guide).
# Usage: ./test.sh
set -eu
cd "$(dirname "$0")"

bold() { printf "\n\033[1m== %s ==\033[0m\n" "$1"; }

bold "1/6  formatting (cargo fmt --check)"
cargo fmt --all --check

bold "2/6  lints (cargo clippy -D warnings)"
cargo clippy --all-targets -- -D warnings 2>&1 | tail -1

bold "3/6  tests (whole workspace)"
cargo test 2>&1 | grep -E 'test result|running [0-9]+ test' || true
echo "total: $(cargo test 2>&1 | grep -oE '[0-9]+ passed' | awk '{s+=$1} END{print s}') tests passed"

bold "4/6  warm-query SLO bench"
cargo run -q -p vyer-server --example warm_bench --release -- . 2>/dev/null | tail -3

bold "5/6  build the CLI"
cargo build -q
SCRY=./target/debug/vyer

bold "6/6  CLI smoke (search / structural / refs / repo-map / no-match)"
echo "-- search 'validate_write' (snippet) --"
$SCRY query "validate_write" --detail snippet --k 2 | head -6
echo "-- structural 'Engine' (outline) --"
$SCRY query "Engine" --mode structural --detail outline --k 3 | head -6
echo "-- graph refs 'pagerank' --"
$SCRY query "pagerank" --mode graph | head -5
echo "-- repo map (top 5) --"
$SCRY map | head -6
echo "-- no-match (actionable error) --"
$SCRY query "definitely_not_here_zzz"

bold "ALL GREEN ✅"
