"""Track A — edit / freshness / safety bench: vyer code_apply vs native write.

Runs on a throwaway COPY of the snake repo (never the real one). Measures:
  E1  latency of an additive symbol edit (add a trailing comment)
  E5  broken-edit handling: vyer must REJECT (re-parse) where native blindly writes
  F1  read-after-write freshness: query the symbol right after writing -> staleness

Native side = the mechanism Claude Code's Edit wraps: read file, string-replace, write.

Run:  python3 benches/track_a/edit_bench.py
"""
import os
import sys
import time
import shutil
import tempfile
import subprocess
import statistics as st

sys.path.insert(0, os.path.dirname(__file__))
from mcp_client import MCPClient

SRC = "playground/snake"
N = 100
WARMUP = 5


def pct(samples, q):
    s = sorted(samples)
    return s[min(int(len(s) * q), len(s) - 1)]


def main():
    tmp = tempfile.mkdtemp(prefix="vyer_bench_")
    repo = os.path.join(tmp, "snake")
    shutil.copytree(SRC, repo)
    print(f"sandbox: {repo}")

    client = MCPClient(["./target/release/vyer", "serve", "--root", repo,
                        "--allow-writes"])
    client.initialize()

    # Two parse-valid variants of is_over() so each apply truly changes bytes.
    body = ("    def is_over(self) -> bool:\n"
            "        return self.over  # bench edit {n}")

    # ---- E1: vyer apply latency (additive symbol edit) -----------------
    def vyer_edit(n):
        _dt, text, _r = client.call_tool(
            "code_apply",
            {"edits": [{"locator": "game.py#is_over",
                        "new_body": body.format(n=n)}]})
        return text

    for i in range(WARMUP):
        vyer_edit(i)
    s_lat = []
    for i in range(N):
        t = time.perf_counter()
        out = vyer_edit(i)
        s_lat.append((time.perf_counter() - t) * 1000.0)
    vyer_ok = "parse=ok" in out or "diff" in out.lower() or "+++" in out

    # ---- E1 native: read-replace-write on a separate file --------------
    gp = os.path.join(repo, "game.py")
    n_lat = []
    with open(gp) as fh:
        original = fh.read()
    for i in range(N):
        t = time.perf_counter()
        with open(gp) as fh:
            txt = fh.read()
        txt2 = txt.replace("return self.over", "return self.over")  # no-op replace
        with open(gp, "w") as fh:
            fh.write(txt2)
        n_lat.append((time.perf_counter() - t) * 1000.0)
    with open(gp, "w") as fh:
        fh.write(original)

    # ---- F1: read-after-write freshness (vyer) -------------------------
    stale = 0
    for i in range(50):
        marker = f"freshness {i}"
        client.call_tool("code_apply", {"edits": [
            {"locator": "game.py#is_over",
             "new_body": f"    def is_over(self) -> bool:\n        return self.over  # {marker}"}]})
        _dt, text, _r = client.call_tool("code", {"queries": [
            {"q": "is_over", "mode": "structural", "detail": "snippet", "k": 1}]})
        if marker not in text:
            stale += 1

    # ---- E5: broken-edit handling (CPython is the ground-truth oracle) --
    import ast
    # Reset is_over to a known-good body first.
    client.call_tool("code_apply", {"edits": [{"locator": "game.py#is_over",
        "new_body": "    def is_over(self) -> bool:\n        return self.over  # reset"}]})
    # vyer: a body CPython cannot parse SHOULD be refused (CLAUDE.md Rule 4).
    _dt, sbroken, _r = client.call_tool("code_apply", {"edits": [
        {"locator": "game.py#is_over",
         "new_body": "    def is_over(self) -> bool:\n        return self.over (((  # broken"}]})
    vyer_claims_ok = "parse=ok" in sbroken            # what vyer reported
    # Did the broken edit actually land on disk?
    with open(gp) as fh:
        after = fh.read()
    try:
        ast.parse(after)
        vyer_file_parses = True
    except SyntaxError:
        vyer_file_parses = False
    # Honest verdict: vyer "rejected" only if the file still parses (edit not written)
    # OR vyer did not claim parse=ok.
    vyer_refused = vyer_file_parses or not vyer_claims_ok
    vyer_false_ok = vyer_claims_ok and not vyer_file_parses  # the bug we found

    # native: blindly writes the broken text -> file no longer parses.
    nfile = os.path.join(repo, "snake_native_broken.py")
    with open(nfile, "w") as fh:
        fh.write("def is_over():\n    return x (((  # broken\n")
    native_parses = subprocess.run(
        [sys.executable, "-c", f"import ast,sys; ast.parse(open('{nfile}').read())"],
        capture_output=True).returncode == 0

    client.close()
    shutil.rmtree(tmp, ignore_errors=True)

    # ---- report --------------------------------------------------------
    print(f"\n{'metric':<34}{'vyer':>14}{'native':>14}")
    print("-" * 62)
    print(f"{'E1 edit p50 (ms)':<34}{pct(s_lat,.5):>14.3f}{pct(n_lat,.5):>14.3f}")
    print(f"{'E1 edit p95 (ms)':<34}{pct(s_lat,.95):>14.3f}{pct(n_lat,.95):>14.3f}")
    print(f"{'E1 edit mean (ms)':<34}{st.mean(s_lat):>14.3f}{st.mean(n_lat):>14.3f}")
    print(f"{'E1 produced valid edit':<34}{str(vyer_ok):>14}{'n/a':>14}")
    print(f"{'F1 stale reads / 50 (lower=better)':<34}{stale:>14}{'n/a':>14}")
    print(f"{'E5 claimed parse=ok':<34}{str(vyer_claims_ok):>14}{'n/a':>14}")
    print(f"{'E5 file actually parses (CPython)':<34}{str(vyer_file_parses):>14}{str(native_parses):>14}")
    print(f"{'E5 FALSE parse=ok (silent bad write)':<34}{str(vyer_false_ok):>14}{'n/a':>14}")

    os.makedirs("results", exist_ok=True)
    with open("results/track_a_edit_xs.csv", "w") as fh:
        fh.write("metric,vyer,native\n")
        fh.write(f"E1_edit_p50_ms,{pct(s_lat,.5):.3f},{pct(n_lat,.5):.3f}\n")
        fh.write(f"E1_edit_p95_ms,{pct(s_lat,.95):.3f},{pct(n_lat,.95):.3f}\n")
        fh.write(f"E1_edit_mean_ms,{st.mean(s_lat):.3f},{st.mean(n_lat):.3f}\n")
        fh.write(f"F1_stale_reads_of_50,{stale},n/a\n")
        fh.write(f"E5_claimed_parse_ok,{vyer_claims_ok},n/a\n")
        fh.write(f"E5_file_actually_parses,{vyer_file_parses},{native_parses}\n")
        fh.write(f"E5_false_parse_ok_silent_bad_write,{vyer_false_ok},n/a\n")
    print("\nwrote results/track_a_edit_xs.csv")


if __name__ == "__main__":
    main()
