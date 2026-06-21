"""Scale benchmark — does vyer stay fast on a large repo?

Generates an N-file synthetic Python codebase with cross-file references, then
measures cold-index time, warm search/read latency, and a repo-wide rename
(touching many files) at that scale.

Run:  python3 benches/track_a/scale_bench.py [n_files]
"""
import os, sys, time, shutil, tempfile, statistics as st
sys.path.insert(0, os.path.abspath(os.path.dirname(__file__)))
from mcp_client import MCPClient

VYER = "./target/release/vyer"


def gen_repo(n):
    tmp = tempfile.mkdtemp(prefix="vyer_scale_")
    repo = os.path.join(tmp, "r")
    os.makedirs(repo)
    # a shared "core" symbol referenced widely, plus per-file unique symbols.
    open(os.path.join(repo, "core.py"), "w").write(
        "def shared_helper(x):\n    return x + 1\n")
    for i in range(n):
        lines = [
            "from core import shared_helper",
            "",
            f"def feature_{i}(a, b):",
            f"    # module {i}",
            "    r = shared_helper(a)",
            f"    return r + b + {i}",
            "",
            f"class Service_{i}:",
            "    def run(self, x):",
            "        return shared_helper(x)",
        ]
        open(os.path.join(repo, f"mod_{i:04d}.py"), "w").write("\n".join(lines) + "\n")
    return tmp, repo


def median_ms(fn, reps=20):
    L = []
    for _ in range(reps):
        t = time.perf_counter()
        fn()
        L.append((time.perf_counter() - t) * 1000)
    return st.median(L)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
    tmp, repo = gen_repo(n)
    total_files = n + 1
    print(f"generated {total_files} files (~{total_files * 10} lines)")

    t0 = time.perf_counter()
    c = MCPClient([VYER, "serve", "--root", repo, "--allow-writes"])
    c.initialize()
    # First query forces the index to be warm; measure cold build via status.
    _dt, _t, _ = c.call_tool("code", {"queries": [{"q": "shared_helper", "mode": "structural", "detail": "locate", "k": 1}]})
    cold = (time.perf_counter() - t0) * 1000
    print(f"cold start + index + first query : {cold:.0f} ms")

    s_lat = median_ms(lambda: c.call_tool("code", {"queries": [
        {"q": "feature_500", "mode": "structural", "detail": "locate", "k": 1}]}))
    print(f"warm structural search (p50)     : {s_lat:.2f} ms")

    r_lat = median_ms(lambda: c.call_tool("code", {"queries": [
        {"path": "mod_0500.py", "detail": "full"}]}))
    print(f"warm read-by-path (p50)          : {r_lat:.2f} ms")

    impact_lat = median_ms(lambda: c.call_tool("code", {"queries": [
        {"q": "shared_helper", "detail": "impact"}]}), reps=5)
    print(f"impact of widely-used symbol (p50): {impact_lat:.2f} ms  "
          f"(blast radius across ~{total_files} files)")

    # repo-wide rename of the widely-referenced symbol (dry-run for clean timing).
    rn_lat = median_ms(lambda: c.call_tool("code_apply", {
        "edits": [{"locator": "core.py#shared_helper", "rename": "shared_helper2"}],
        "dry_run": True}), reps=5)
    # one real rename to confirm it actually rewrites the whole repo.
    _dt, txt, _ = c.call_tool("code_apply", {
        "edits": [{"locator": "core.py#shared_helper", "rename": "shared_helper2"}]})
    occ = txt.splitlines()[0]
    print(f"repo-wide rename dry-run (p50)    : {rn_lat:.2f} ms")
    print(f"  -> {occ}")

    c.close()
    shutil.rmtree(tmp, ignore_errors=True)
    print("\nSLO targets: locate/outline p50<30ms p95<120ms; snippet p50<50ms — "
          "compare the warm numbers above.")


if __name__ == "__main__":
    main()
