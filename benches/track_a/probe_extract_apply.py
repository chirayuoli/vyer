"""Battery B/C/D — symbol extraction, multi-language, and apply edge cases.

Runs against a throwaway sandbox so it never touches the real repo. Prints a
compact pass/miss report for each probe.
"""
import os, sys, shutil, tempfile, json
sys.path.insert(0, os.path.dirname(__file__))
from mcp_client import MCPClient

TRICKY = '''"""Tricky module for symbol-extraction probing."""
import functools

CONST = 42
_lambda_fn = lambda x: x + 1

def module_func(a, b=2):
    def nested():
        return a + b
    return nested

async def fetch(url):
    return url

@functools.lru_cache
def decorated(n):
    return n * 2

class Outer:
    class_var = 1

    @property
    def value(self):
        return self.class_var

    @staticmethod
    def helper():
        return 0

    async def amethod(self):
        return 1

    def gen(self):
        yield from range(3)

    class Inner:
        def inner_m(self):
            return 2

def dup():
    return "first"

def dup():
    return "second"
'''

RS = '''pub struct Point { x: i32, y: i32 }
impl Point {
    pub fn new(x: i32, y: i32) -> Self { Point { x, y } }
}
pub fn rust_free() -> i32 { 7 }
'''
JS = '''export function jsFunc(a) { return a + 1; }
class JsClass { method() { return 2; } }
const arrow = (x) => x * 2;
'''
TS = '''export function tsFunc(a: number): number { return a + 1; }
interface Foo { bar(): void; }
class TsClass { method(): number { return 2; } }
'''
GO = '''package main
type Shape struct { side int }
func (s Shape) Area() int { return s.side * s.side }
func GoFree() int { return 9 }
'''


def found(client, name, scope):
    _dt, text, _r = client.call_tool("code", {"queries": [
        {"q": name, "mode": "structural", "detail": "locate", "k": 10,
         "path_scope": [scope]}]})
    # A structural hit on the symbol name = its locator id appears as PATH#name
    return f"#{name}@" in text or f"#{name} " in text or f"#{name}\n" in text, text


def main():
    tmp = tempfile.mkdtemp(prefix="vyer_probe_")
    repo = os.path.join(tmp, "repo")
    os.makedirs(repo)
    for fn, content in [("tricky.py", TRICKY), ("sample.rs", RS),
                        ("sample.js", JS), ("sample.ts", TS), ("sample.go", GO)]:
        with open(os.path.join(repo, fn), "w") as fh:
            fh.write(content)

    c = MCPClient(["./target/release/vyer", "serve", "--root", repo,
                   "--allow-writes"])
    c.initialize()

    print("== B: Python symbol extraction ==")
    py_syms = ["module_func", "nested", "fetch", "decorated", "Outer", "value",
               "helper", "amethod", "gen", "Inner", "inner_m", "dup", "_lambda_fn"]
    for s in py_syms:
        ok, _ = found(c, s, "tricky.py")
        print(f"  {'OK ' if ok else 'MISS'} {s}")

    print("== D: multi-language extraction ==")
    for s, scope in [("Point", "sample.rs"), ("new", "sample.rs"),
                     ("rust_free", "sample.rs"), ("jsFunc", "sample.js"),
                     ("JsClass", "sample.js"), ("arrow", "sample.js"),
                     ("tsFunc", "sample.ts"), ("Foo", "sample.ts"),
                     ("TsClass", "sample.ts"), ("Shape", "sample.go"),
                     ("Area", "sample.go"), ("GoFree", "sample.go")]:
        ok, _ = found(c, s, scope)
        print(f"  {'OK ' if ok else 'MISS'} {scope:<12} {s}")

    print("== C: apply edge cases ==")

    def apply(edits, dry=False, label=""):
        _dt, text, _r = c.call_tool("code_apply",
                                    {"edits": edits, "dry_run": dry})
        head = text.strip().splitlines()
        status = "parse=ok" if "parse=ok" in text else (
            "diff/dry" if "+++" in text or "---" in text else "ERR/other")
        first = head[0][:70] if head else ""
        print(f"  [{label}] -> {status} | {first}")
        return text

    # C1 property with decorator
    apply([{"locator": "tricky.py#value",
            "new_body": "    @property\n    def value(self):\n        return self.class_var  # edited"}],
          label="C1 property")
    # C2 async method
    apply([{"locator": "tricky.py#amethod",
            "new_body": "    async def amethod(self):\n        return 1  # edited"}],
          label="C2 async")
    # C3 nested function
    apply([{"locator": "tricky.py#nested",
            "new_body": "    def nested():\n        return a + b  # edited"}],
          label="C3 nested")
    # C4 overloaded name: which dup?
    apply([{"locator": "tricky.py#dup",
            "new_body": 'def dup():\n    return "EDITED"'}],
          dry=True, label="C4 dup(overload) dry")
    # C5 @Lstart-end locator form
    apply([{"locator": "tricky.py#fetch@L13-14",
            "new_body": "async def fetch(url):\n    return url  # edited"}],
          dry=True, label="C5 @Lrange")
    # C6 two edits one call (line-shift interaction)
    apply([{"locator": "tricky.py#module_func",
            "new_body": "def module_func(a, b=2):\n    def nested():\n        return a + b\n    return nested  # e1\n    # pad\n    # pad2"},
           {"locator": "tricky.py#decorated",
            "new_body": "@functools.lru_cache\ndef decorated(n):\n    return n * 2  # e2"}],
          label="C6 two-edits")
    # C7 idempotent re-apply (same body twice)
    b = "@functools.lru_cache\ndef decorated(n):\n    return n * 2  # e2"
    apply([{"locator": "tricky.py#decorated", "new_body": b}], label="C7 re-apply-1")
    apply([{"locator": "tricky.py#decorated", "new_body": b}], label="C7 re-apply-2")
    # C8 lazy_edit fallback (Phase 6 — expect rejection)
    apply([{"locator": "tricky.py#helper",
            "lazy_edit": "    @staticmethod\n    def helper():\n        # ... existing code ...\n        return 0"}],
          label="C8 lazy_edit")

    c.close()
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
