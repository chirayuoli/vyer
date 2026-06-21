"""Track A — mechanism micro-bench: vyer (warm MCP) vs native mechanisms.

Read + search on the snake (XS) repo. Reports latency percentiles and, crucially,
output size (a faithful proxy for context-token cost — the metric vyer's thesis
lives or dies on). Native side uses the same engines Claude Code's tools wrap:
ripgrep for Grep, fs read + line-number formatting for Read.

Run:  python3 benches/track_a/run_track_a.py
"""
import os
import sys
import subprocess
import time
import statistics as st

sys.path.insert(0, os.path.dirname(__file__))
from mcp_client import MCPClient

ROOT = "playground/snake"
VYER = ["./target/release/vyer", "serve", "--root", ROOT]
N = 200          # measured reps per case
WARMUP = 10      # discarded reps
RG = "rg"


def pct(samples, q):
    s = sorted(samples)
    return s[min(int(len(s) * q), len(s) - 1)]


def approx_tokens(text):
    # ~4 chars/token is the standard rough proxy; the *ratio* is what matters
    # and is tokenizer-insensitive.
    return max(1, round(len(text) / 4))


def bench(fn, n=N, warmup=WARMUP):
    for _ in range(warmup):
        fn()
    lat, out_bytes = [], None
    for _ in range(n):
        t = time.perf_counter()
        text = fn()
        lat.append((time.perf_counter() - t) * 1000.0)
        out_bytes = len(text)  # deterministic per case; record last
    return lat, out_bytes


# ---- native mechanisms ------------------------------------------------
def native_rg(pattern, extra=None):
    def f():
        args = [RG, "--line-number", "--no-heading", pattern, ROOT]
        if extra:
            args[1:1] = extra
        r = subprocess.run(args, capture_output=True, text=True)
        return r.stdout
    return f


def native_read_file(path):
    full = os.path.join(ROOT, path)

    def f():
        with open(full, "r") as fh:
            lines = fh.readlines()
        # Mirror Claude Code's Read: cat -n style line numbering.
        return "".join(f"{i+1}\t{ln}" for i, ln in enumerate(lines))
    return f


# ---- vyer via warm MCP ------------------------------------------------
def make_vyer_call(client, args):
    def f():
        _dt, text, _r = client.call_tool("code", {"queries": [args]})
        return text
    return f


def main():
    client = MCPClient(VYER)
    client.initialize()

    cases = []
    # (id, capability, vyer_query, native_fn, note)
    cases.append((
        "S1_lexical_exact", "search",
        {"q": "collides_self", "mode": "lexical", "detail": "locate", "k": 8},
        native_rg("collides_self"),
        "exact identifier; both should nail it",
    ))
    cases.append((
        "S2_structural_def", "search",
        {"q": "Snake", "mode": "structural", "detail": "outline", "k": 8},
        native_rg(r"class Snake\b"),
        "symbol definition; native grep is a regex approximation",
    ))
    cases.append((
        "S3_usages", "search",
        {"q": "choose_direction", "mode": "auto", "detail": "locate", "k": 8},
        native_rg("choose_direction"),
        "where-used; vyer locate vs grep lines",
    ))
    cases.append((
        "R1_whole_file", "read",
        {"q": "Game", "mode": "structural", "detail": "full", "k": 1},
        native_read_file("game.py"),
        "read a file's main class vs native full-file Read (vyer has no read-by-path)",
    ))
    cases.append((
        "R3_one_function", "read",
        {"q": "choose_direction", "mode": "structural", "detail": "snippet", "k": 1},
        native_read_file("ai.py"),
        "want ONE function: vyer snippet returns just it; native Read returns whole ai.py",
    ))
    cases.append((
        "R4_locate_only", "read",
        {"q": "choose_direction", "mode": "structural", "detail": "locate", "k": 1},
        native_read_file("ai.py"),
        "just need the location: vyer locator vs native must read the whole file",
    ))

    rows = []
    print(f"{'case':<20}{'side':<8}{'p50ms':>8}{'p95ms':>8}{'bytes':>8}{'~tok':>7}")
    print("-" * 59)
    for cid, cap, sq, nfn, note in cases:
        s_lat, s_bytes = bench(make_vyer_call(client, sq))
        n_lat, n_bytes = bench(nfn)
        for side, lat, b in (("vyer", s_lat, s_bytes), ("native", n_lat, n_bytes)):
            p50, p95 = pct(lat, .50), pct(lat, .95)
            tok = approx_tokens("x" * b)
            print(f"{cid:<20}{side:<8}{p50:>8.2f}{p95:>8.2f}{b:>8}{tok:>7}")
            rows.append((cid, cap, side, f"{p50:.3f}", f"{p95:.3f}",
                         f"{st.mean(lat):.3f}", b, tok, note))
        # token ratio (native/vyer) — the headline
        ratio = s_bytes and (n_bytes / s_bytes)
        print(f"{'':<20}{'-> token ratio native/vyer =':<28}{ratio:>6.2f}x  ({note})")
    client.close()

    # write CSV
    out = "results/track_a_xs.csv"
    os.makedirs("results", exist_ok=True)
    with open(out, "w") as fh:
        fh.write("case,capability,side,p50_ms,p95_ms,mean_ms,out_bytes,approx_tokens,note\n")
        for r in rows:
            fh.write(",".join(f'"{x}"' for x in r) + "\n")
    print(f"\nwrote {out}  (N={N} reps, warmup={WARMUP}, repo={ROOT})")


if __name__ == "__main__":
    main()
