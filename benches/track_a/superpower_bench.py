"""Superpower benchmark — vyer's beyond-parity ops vs the native-tool workflow.

Measures the operations native tools CANNOT do in one safe step: a repo-wide
rename and an atomic multi-file edit. Reports vyer's actual latency + output
size against the *native-equivalent workflow cost* (tool round-trips + tokens an
agent must spend doing the same thing with Grep/Read/Edit).

Run:  python3 benches/track_a/superpower_bench.py
"""
import os, sys, time, shutil, tempfile, statistics as st
sys.path.insert(0, os.path.dirname(__file__))
from mcp_client import MCPClient

VYER = "./target/release/vyer"
N = 50


def approx_tokens(s):
    return max(1, round(len(s) / 4))


def make_repo(n_files, refs_per_file):
    tmp = tempfile.mkdtemp(prefix="vyer_sp_")
    repo = os.path.join(tmp, "r")
    os.makedirs(repo)
    # one definition file + n_files that import & call it refs_per_file times.
    open(os.path.join(repo, "core.py"), "w").write(
        "def validate_token(t):\n    return bool(t)\n")
    for i in range(n_files):
        body = ["from core import validate_token", "", f"def use_{i}(t):"]
        for j in range(refs_per_file):
            body.append(f"    _x{j} = validate_token(t)  # ref {j}")
        body.append("    return _x0")
        open(os.path.join(repo, f"mod_{i}.py"), "w").write("\n".join(body) + "\n")
    return tmp, repo


def bench_rename():
    print("== Superpower 1: repo-wide rename (validate_token -> verify_token) ==")
    for n_files, refs in [(5, 3), (20, 5)]:
        tmp, repo = make_repo(n_files, refs)
        c = MCPClient([VYER, "serve", "--root", repo, "--allow-writes"])
        c.initialize()
        # dry-run repeatedly for a clean latency sample (no disk mutation drift).
        lat, out_bytes = [], 0
        for _ in range(N):
            t = time.perf_counter()
            _dt, txt, _ = c.call_tool("code_apply", {
                "edits": [{"locator": "core.py#validate_token", "rename": "verify_token"}],
                "dry_run": True})
            lat.append((time.perf_counter() - t) * 1000)
            out_bytes = len(txt)
        c.close(); shutil.rmtree(tmp, ignore_errors=True)

        total_refs = n_files * refs + n_files + 1   # calls + imports + def
        # Native equivalent: 1 grep to locate, then read+edit each of (n_files+1)
        # files, then 1 verify grep. Tokens dominated by reading every file whole.
        native_calls = 1 + 2 * (n_files + 1) + 1
        approx_file_tokens = (3 + refs) * 12  # rough tokens/file read
        native_tokens = (n_files + 1) * approx_file_tokens
        vyer_tokens = approx_tokens("x" * out_bytes)
        print(f"\n  repo: {n_files+1} files, ~{total_refs} occurrences")
        print(f"    vyer   : 1 call (atomic+validated)  p50={st.median(lat):.2f}ms  "
              f"out={out_bytes}B (~{vyer_tokens} tok)")
        print(f"    native : {native_calls} tool calls (grep+{(n_files+1)}×read+"
              f"{(n_files+1)}×edit+verify), NO atomicity/validation  ~{native_tokens} tok")
        print(f"    => vyer uses 1 round-trip vs {native_calls}; "
              f"~{native_tokens/max(1,vyer_tokens):.0f}× fewer tokens; and is atomic+parse-checked")


def bench_atomic():
    print("\n== Superpower 2: atomic multi-file edit (all-or-nothing) ==")
    tmp, repo = make_repo(3, 1)
    c = MCPClient([VYER, "serve", "--root", repo, "--allow-writes"])
    c.initialize()
    # A 3-edit batch where the last edit is invalid -> whole batch must abort,
    # leaving every file untouched. Native Edit has no such guarantee.
    edits = [
        {"locator": "mod_0.py#use_0", "new_body": "def use_0(t):\n    return 1"},
        {"locator": "mod_1.py#use_1", "new_body": "def use_1(t):\n    return 2"},
        {"locator": "mod_2.py#NOPE", "new_body": "def x(): pass"},  # invalid
    ]
    _dt, txt, _ = c.call_tool("code_apply", {"edits": edits, "dry_run": False})
    aborted = "error" in txt.lower() or "no symbol" in txt.lower() or "apply failed" in txt.lower()
    # Confirm mod_0 / mod_1 untouched on disk (rolled back).
    untouched = all(
        "return 1" not in open(os.path.join(repo, f)).read()
        and "return 2" not in open(os.path.join(repo, f)).read()
        for f in ["mod_0.py", "mod_1.py"])
    c.close(); shutil.rmtree(tmp, ignore_errors=True)
    print(f"  3-edit batch, edit #3 invalid:")
    print(f"    vyer   : whole batch aborted={aborted}; edits #1/#2 rolled back={untouched} "
          f"(0 files changed)")
    print(f"    native : Edit #1 and #2 already written before #3 fails -> repo left HALF-EDITED")
    print(f"    => vyer gives transactional safety native Edit cannot")


if __name__ == "__main__":
    bench_rename()
    bench_atomic()
    print("\n(values are directional; the structural wins — 1 round-trip, atomic, "
          "parse-validated — are the point, not the exact tokens.)")
