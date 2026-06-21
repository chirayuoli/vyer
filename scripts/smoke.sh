#!/usr/bin/env bash
# Smoke-test the real release binary across every detail/mode/filter against THIS
# repo. Catches regressions in the CLI + binary wiring that unit tests don't.
# Usage: cargo build --release -p vyer-server && bash scripts/smoke.sh
set -u
BIN="${SCRY_BIN:-./target/release/vyer}"
pass=0
fail=0

# check <description> <expect-substring> -- <vyer args...>
check() {
  local desc="$1" expect="$2"; shift 2
  [ "$1" = "--" ] && shift
  local out
  out="$("$BIN" "$@" 2>&1)"
  if printf '%s' "$out" | grep -qiE "thread .main. panicked|RUST_BACKTRACE"; then
    echo "FAIL  $desc  (panic)"; fail=$((fail+1)); return
  fi
  if printf '%s' "$out" | grep -qF "$expect"; then
    pass=$((pass+1))
  else
    echo "FAIL  $desc  (missing: $expect)"; echo "      got: $(printf '%s' "$out" | head -1)"
    fail=$((fail+1))
  fi
}

# --- search modes ---
check "mode=auto"        "code/result" -- query "validate_token" --detail snippet
check "mode=lexical"     "code/result" -- query "validate_token" --mode lexical --detail locate
check "mode=structural"  "code/result" -- query "code" --mode structural --detail locate
check "mode=semantic"    "code/result" -- query "check token valid" --mode semantic --detail locate
check "mode=ast"         "code/result" -- query "(function_item) @f" --mode ast --lang rust --detail locate

# --- detail views ---
check "detail=outline"   "code/result" -- query "validate_token" --detail outline
check "detail=full"      "code/result" -- query "validate_token" --detail full
check "detail=refs"      "graph=partial" -- query "validate_token" --detail refs
check "detail=impact"    "impact of"    -- query "validate_token" --detail impact
check "detail=context"   "context for"  -- query "ast_spans" --detail context
check "detail=count"     "matches on"   -- query "fn" --detail count
check "detail=tree"      "files"        -- query "" --detail tree --path-scope "crates/vyer-core/**"
check "detail=ast (sym)" "function_item" -- query "lost_in_the_middle" --path "crates/vyer-core/src/lib.rs" --detail ast
check "subtree outline"  "code/result"  -- query "" --detail outline --path-scope "crates/vyer-core/**"

# --- read by path ---
check "read whole file"  "1: "          -- query "x" --path "crates/vyer-core/src/lib.rs" --detail full
check "read head"        "1: "          -- query "x" --path "crates/vyer-core/src/lib.rs" --lines "-20"

# --- boolean + filters ---
check "boolean all_of"   "matching lines" -- query "" --detail count --all unwrap --all expect
check "exclusion glob"   "code/result"  -- query "fn" --mode lexical --detail count --path-scope "crates/**" --path-scope "!**/tests/**"
check "multi-lang"       "code/result"  -- query "fn" --mode lexical --detail locate --lang "rust,go"

# --- apply (dry-run, writes nothing) ---
check "apply rename (dry)" "rename"      -- apply --locator "crates/vyer-core/src/lib.rs#est_tokens" --rename est_tokens2
check "apply word-rename (dry)" "@@"   -- apply --locator "crates/vyer-server/src/engine.rs#code" --anchor low_confidence --replace lc --word
check "apply undo msg"     "MCP daemon"   -- apply --undo 1

# --- resources / cli subcommands ---
check "status"           "vyer/status"  -- status
check "repo map"         ""             -- map

echo "----"
echo "smoke: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
