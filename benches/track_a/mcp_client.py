"""Minimal line-delimited JSON-RPC client to drive the vyer stdio MCP server.

Track A harness uses this to time `tools/call` against a *persistent* (warm-core)
server process — which is what an agent actually experiences, unlike the CLI's
cold index-per-call path.
"""
import json
import subprocess
import time
import threading


class MCPClient:
    def __init__(self, cmd):
        # Spawn the server; stderr kept separate so it never corrupts the JSON stream.
        self.p = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, bufsize=0,
        )
        self._id = 0
        self._lock = threading.Lock()

    def _send(self, obj):
        line = (json.dumps(obj) + "\n").encode()
        self.p.stdin.write(line)
        self.p.stdin.flush()

    def _read_msg(self):
        # Line-delimited JSON: read until we get a parseable object with our needs.
        while True:
            line = self.p.stdout.readline()
            if not line:
                err = self.p.stderr.read().decode(errors="replace")
                raise RuntimeError(f"server closed stdout. stderr:\n{err}")
            line = line.strip()
            if not line:
                continue
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                # Non-JSON banner line on stdout (shouldn't happen) — skip it.
                continue

    def request(self, method, params=None):
        with self._lock:
            self._id += 1
            rid = self._id
            self._send({"jsonrpc": "2.0", "id": rid, "method": method,
                        "params": params or {}})
            # Read until the matching response id (skip notifications).
            while True:
                msg = self._read_msg()
                if msg.get("id") == rid:
                    return msg

    def notify(self, method, params=None):
        with self._lock:
            self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def initialize(self):
        r = self.request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "vyer-bench", "version": "0.1"},
        })
        self.notify("notifications/initialized")
        return r

    def call_tool(self, name, arguments):
        """Return (elapsed_ms, response_text). Times the full round-trip."""
        t = time.perf_counter()
        r = self.request("tools/call", {"name": name, "arguments": arguments})
        dt = (time.perf_counter() - t) * 1000.0
        text = _extract_text(r)
        return dt, text, r

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.terminate()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def _extract_text(resp):
    """Pull the concatenated text content out of an MCP tools/call result."""
    res = resp.get("result", {})
    if isinstance(res, dict):
        content = res.get("content", [])
        parts = []
        for c in content:
            if isinstance(c, dict) and c.get("type") == "text":
                parts.append(c.get("text", ""))
        if parts:
            return "\n".join(parts)
    # Fallback: stringify whatever came back (errors, structured content).
    return json.dumps(resp.get("result", resp.get("error", "")))
