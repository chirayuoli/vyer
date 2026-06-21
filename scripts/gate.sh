#!/usr/bin/env bash
# One-command full verification gate for Vyer. Runs every layer that must stay
# green before a release / after a change, in order, failing fast. Mirrors the
# Definition of Done (CLAUDE.md §11):
#   build → workspace tests → clippy(-D warnings) → fmt --check → binary smoke → warm SLO.
#
# Usage:  bash scripts/gate.sh
# Exit:   0 only if every layer passes; non-zero (and a clear FAIL line) otherwise.
set -uo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$1"; exit 1; }

step "build (release)"
cargo build --release -p vyer-server || fail "release build"

step "workspace tests"
# Sum the per-crate "ok" counts; any failure makes `cargo test` exit non-zero.
cargo test --workspace 2>&1 | tee /tmp/vyer_gate_test.log | grep -E "test result:" || true
grep -qE "test result: FAILED|error\[" /tmp/vyer_gate_test.log && fail "tests"
passed=$(grep -oE "test result: ok\. [0-9]+ passed" /tmp/vyer_gate_test.log | awk '{s+=$4} END {print s}')
printf 'tests passed: %s\n' "${passed:-0}"

step "clippy (-D warnings)"
cargo clippy --all-targets -- -D warnings 2>&1 | grep -E "^(warning|error)" && fail "clippy" || echo "clean"

step "fmt --check"
cargo fmt --all -- --check >/dev/null 2>&1 || fail "fmt (run: cargo fmt --all)"
echo "clean"

step "binary smoke"
bash scripts/smoke.sh || fail "smoke"

step "warm SLO (all modes)"
# SCRY-098: ASSERT the §7 thresholds, don't just print them — an unenforced SLO lets a
# perf regression (e.g. p50 jumping to 100ms) slip through green. Thresholds are the §7
# SLO (p50<30ms, p95<120ms): loose enough that load-induced tail noise won't flake (the
# median is robust; p95 here is ~20ms even under load), tight enough to catch a gross
# regression. This repo is small so the numbers run well under — the guard is the point.
warm=$(cargo run -q -p vyer-server --example warm_bench --release 2>/dev/null | grep "warm query")
[ -n "$warm" ] || fail "warm_bench did not run"
printf '%s\n' "$warm"
p50=$(printf '%s' "$warm" | grep -oE "p50=[0-9.]+" | cut -d= -f2)
p95=$(printf '%s' "$warm" | grep -oE "p95=[0-9.]+" | cut -d= -f2)
awk -v v="$p50" 'BEGIN{exit !(v+0 < 30)}' || fail "warm p50=${p50}ms exceeds §7 SLO (<30ms)"
awk -v v="$p95" 'BEGIN{exit !(v+0 < 120)}' || fail "warm p95=${p95}ms exceeds §7 SLO (<120ms)"
printf 'warm SLO OK: p50=%sms<30  p95=%sms<120\n' "$p50" "$p95"

printf '\n\033[32mGATE PASSED — every layer green.\033[0m\n'
