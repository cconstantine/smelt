"""Shared by scripts/pm/tests/*: run a PM script as a subprocess against a
fake smelt (scripts/pm/fake_smelt.py) and a fake smelt-mcp."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PM = os.path.dirname(HERE)
sys.path.insert(0, PM)

import fake_smelt  # noqa: E402

CLIENT = os.path.join(PM, "smelt-api")
WAITER = os.path.join(PM, "smelt-wait")

# Environment variables a run must not inherit from the seat.
SCRUBBED = ("SMELT_API_URL", "SMELT_API_HOST", "SME", "SMELT_KEY", "http_proxy", "https_proxy",
            "HTTP_PROXY", "HTTPS_PROXY", "all_proxy", "ALL_PROXY", "SMELT_MCP_BIN")


def base_env(**extra):
    env = {k: v for k, v in os.environ.items() if k not in SCRUBBED}
    env["no_proxy"] = "*"
    env.update({k: v for k, v in extra.items() if v is not None})
    return env


class FakeCase(unittest.TestCase):
    """Each test gets its own fake smelt on a free port."""

    def setUp(self):
        self.fake = fake_smelt.FakeSmelt().start()
        self.st = self.fake.state
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.fake.stop)

    def env(self, **extra):
        extra.setdefault("SMELT_API_URL", self.fake.url)
        return base_env(**extra)

    def run_script(self, script, *args, env=None, stdin=None, timeout=30):
        return subprocess.run(
            [sys.executable, "-I", "-B", script, *args],
            env=env if env is not None else self.env(),
            input=stdin,
            capture_output=True,
            text=True,
            timeout=timeout,
        )

    def api(self, *args, **kw):
        return self.run_script(CLIENT, *args, **kw)

    def tmpfile(self, name, text):
        path = os.path.join(self.tmp.name, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return path

    def sent(self, method=None):
        """(method, path) of every request the fake got, or only `method`'s."""
        return [(r["method"], r["path"]) for r in self.fake.requests()
                if method is None or r["method"] == method]

    def add_conv(self, cid, parent=None, **kw):
        self.st.nodes.append(fake_smelt.node(cid, parent, **kw))

    def fake_mcp(self, script):
        """A fake smelt-mcp on a fresh bin dir: `script` maps a tool name to
        a list of JSON results (strings), handed out in order, the last one
        repeated; a result of None makes the call fail (exit 1)."""
        bindir = os.path.join(self.tmp.name, "bin")
        os.makedirs(bindir, exist_ok=True)
        data = os.path.join(self.tmp.name, "mcp.json")
        with open(data, "w") as f:
            json.dump({"script": script, "calls": {}}, f)
        exe = os.path.join(bindir, "smelt-mcp")
        with open(exe, "w") as f:
            f.write(FAKE_MCP.replace("@DATA@", data))
        os.chmod(exe, 0o755)
        return bindir, data


FAKE_MCP = r'''#!/usr/bin/env python3
import json, sys
data = "@DATA@"
with open(data) as f:
    d = json.load(f)
if len(sys.argv) < 4 or sys.argv[1] != "call":
    sys.exit(2)
tool = sys.argv[2]
args = json.loads(sys.argv[3])
log = d.setdefault("log", [])
log.append([tool, args])
results = d["script"].get(tool, [])
n = d["calls"].get(tool, 0)
d["calls"][tool] = n + 1
with open(data, "w") as f:
    json.dump(d, f)
if not results:
    sys.exit(1)
r = results[min(n, len(results) - 1)]
if r is None:
    print("call failed", file=sys.stderr)
    sys.exit(1)
print(json.dumps({"content": [{"type": "text", "text": r}], "isError": False}))
'''
