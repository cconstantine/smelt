"""scripts/test-pm-live's checks (SME-460): smelt-api and smelt-wait against a
real check server on a scratch database, with scripts/pm/mock_upstream.py
as the model. scripts/test-pm-live starts and stops the server; this runs
the steps and exits non-zero on the first that fails.

The harness's own requests (making the mock provider, reading the tree to
wait for a turn to end) are test setup, outside the PM's allowlist.
"""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
PM = os.path.dirname(HERE)
CLIENT = os.path.join(PM, "smelt-api")
WAITER = os.path.join(PM, "smelt-wait")
URL = os.environ.get("SMELT_API_URL", "http://localhost:8081")
MODEL = "mock-model"
MARK = "PM-LIVE-BRIEF-MARKER"
ASK_ID = "toolu_pmlive_ask"
NOTIFY_ID = "toolu_pmlive_notify"
RULES = [
    {"when": MARK, "tool": "ask_user", "id": ASK_ID, "input": {"questions": [
        {"question": "Which way?", "header": "Way", "options": [{"label": "Left"}, {"label": "Right"}]}]}},
    {"when": ASK_ID, "tool": "notify_user", "id": NOTIFY_ID, "input": {"message": "pm-live done"}},
]

ENV = {k: v for k, v in os.environ.items() if k not in ("SMELT_API_HOST", "SME", "SMELT_KEY")}
ENV.update({"SMELT_API_URL": URL, "SMELT_WAIT_POLL_SECS": "0.5", "no_proxy": "*"})
STEP = [0]


def step(text):
    STEP[0] += 1
    print(f"== {STEP[0]}. {text}", flush=True)


def fail(text):
    print(f"FAILED: {text}", flush=True)
    sys.exit(1)


def check(cond, text):
    if not cond:
        fail(text)


def api(*args, want=0):
    r = subprocess.run([sys.executable, "-I", "-B", CLIENT, *map(str, args)], env=ENV, capture_output=True,
                       text=True, timeout=120)
    print(f"$ smelt-api {' '.join(map(str, args))}  -> exit {r.returncode}")
    for line in (r.stdout + r.stderr).splitlines()[:30]:
        print(f"  | {line}")
    if want is not None:
        check(r.returncode == want, f"smelt-api {args[0]} exited {r.returncode}, wanted {want}")
    return r


def raw(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(URL + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.loads(resp.read() or b"null")


class Waiter:
    def __init__(self, *args):
        self.args = args
        self.proc = subprocess.Popen([sys.executable, "-I", "-B", WAITER, *map(str, args)], env=ENV,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        print(f"$ smelt-wait {' '.join(map(str, args))} &")

    def baseline(self, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = self.proc.stderr.readline()
            if not line:
                break
            print(f"  ! {line.rstrip()}")
            if "baseline taken" in line:
                return
        self.proc.kill()
        fail("smelt-wait took no baseline")

    def woke(self, timeout=120):
        try:
            out, err = self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.communicate()
            fail(f"smelt-wait {self.args} didn't wake in {timeout} s")
        print(f"  -> exit {self.proc.returncode}: {out.strip()}")
        check(self.proc.returncode == 0 and len(out.splitlines()) == 1, "smelt-wait: one wake line, exit 0")
        return out.strip()


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_idle(conv, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        node = next((n for n in raw("GET", "/api/conversations/tree") if n["id"] == conv), None)
        if node and not node["busy"]:
            return
        time.sleep(0.5)
    fail(f"conversation {conv} still busy after {timeout} s")


def main():
    tmp = tempfile.mkdtemp(prefix="pm-live-")
    rules = os.path.join(tmp, "rules.json")
    with open(rules, "w") as f:
        json.dump(RULES, f)
    port = free_port()
    mock = subprocess.Popen([sys.executable, "-I", "-B", os.path.join(PM, "mock_upstream.py"), str(port), rules],
                            stdout=subprocess.PIPE, text=True)
    try:
        check("mock upstream on" in mock.stdout.readline(), "the mock upstream started")
        run(port, tmp)
    finally:
        mock.kill()
        mock.wait(10)
    print("test-pm-live: every step passed")


def run(port, tmp):
    step("where: the check server answers")
    api("where")

    step("the harness makes the mock provider (raw POST /api/providers: setup, not the PM's)")
    provider = raw("POST", "/api/providers", {"input": {
        "name": "pm-live mock", "kind": "anthropic", "base_url": f"http://127.0.0.1:{port}", "auth_kind": "api_key",
        "secret": "pm-live-test-key", "prompt_caching": False, "price_catalog_provider": None,
        "keep_reasoning": False, "max_concurrent_requests": None}})
    pid = provider["id"]
    r = api("providers")
    check(f"{pid} anthropic pm-live mock" in r.stdout.splitlines(), "providers lists the mock as id kind name")
    check("127.0.0.1" not in r.stdout, "providers doesn't print base_url")

    step("create: a new root conversation")
    r = api("create")
    conv = int(r.stdout.splitlines()[0])
    r = api("tree", conv, "--json")
    rows = json.loads(r.stdout)
    check(rows[0]["id"] == conv and rows[0]["parent_id"] is None, "tree shows it as a root")

    step("model-set reads Chosen back, and model agrees")
    r = api("model-set", conv, pid, MODEL)
    check('"Chosen"' in r.stdout, "model-set printed Chosen")
    model = json.loads(api("model", conv).stdout)
    check(model["state"] == "Chosen" and model["choice"]["provider_id"] == pid and model["choice"]["model"] == MODEL,
          "model reads Chosen with the mock")

    step("attach with a bogus URL: the server's error reaches the PM")
    r = api("attach", conv, "not a repo url", want=None)
    check(r.returncode == 1 and "HTTP" in r.stderr, "attach exits 1 with the server's answer")

    step("send --expect-model the brief; smelt-wait wakes on the ask_user")
    brief = os.path.join(tmp, "brief.md")
    with open(brief, "w") as f:
        f.write(f"{MARK}: you are a test Driver.\n")
    w = Waiter(conv, "45", "5", "--no-github")
    w.baseline()
    api("send", conv, brief, "--expect-model", f"{pid}:{MODEL}")
    check(w.woke() == f"WAITING_FOR_USER {conv}", "smelt-wait woke with WAITING_FOR_USER")

    step("question shows it; a wrong label is refused locally")
    q = json.loads(api("question", conv).stdout)
    check(q["tool_use_id"] == ASK_ID, "question names the mock's tool_use_id")
    api("answer", conv, ASK_ID, "Up", want=1)
    check(json.loads(api("question", conv).stdout)["tool_use_id"] == ASK_ID, "the question still waits")

    step("a fresh smelt-wait (--known-waiting) wakes on the notify_user the answer leads to")
    w = Waiter(conv, "45", "5", "--no-github", "--known-waiting", conv)
    w.baseline()
    api("answer", conv, ASK_ID, "Left")
    wake = w.woke()
    check(wake.startswith("TOOL notify_user") and "pm-live done" in wake, "smelt-wait woke with TOOL notify_user")
    wait_idle(conv)

    step("nothing happening: smelt-wait wakes with STALL")
    check(Waiter(conv, "0.05", "5", "--no-github").woke(timeout=60).startswith("STALL 0.05"), "STALL")

    step("pods, messages and question parse the real shapes")
    api("pods")
    r = api("messages", conv, "--last", "10")
    check("notify_user" in r.stdout and "tool_result" in r.stdout, "messages shows the calls and results")
    msgs = json.loads(api("messages", conv, "--json", "--last", "50").stdout)
    check(any(m["role"] == "assistant" for m in msgs), "messages --json has the assistant's turns")
    check(api("question", conv).stdout.strip() == "none", "no question waits now")
    api("tree", conv)


if __name__ == "__main__":
    main()
