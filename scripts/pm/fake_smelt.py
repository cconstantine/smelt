"""A fake smelt for scripts/test-pm (SME-460): a stdlib HTTP server speaking
the routes scripts/pm/smelt_api.py's table names, from an in-memory state,
and recording every request it gets (method, path, headers, body).

Used by the self-tests only. `FakeSmelt().start()` serves on a free
127.0.0.1 port in a thread; the test changes `state` between calls.
The shapes follow the real server functions (src/api/chat.rs, agents.rs,
questions.rs, pods.rs, providers.rs); scripts/test-pm-live checks the real
ones.
"""

import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STAMP = "2026-11-11T12:00:00"


def node(id, parent_id=None, **kw):
    out = {
        "id": id,
        "title": kw.pop("title", f"conversation {id}"),
        "created_at": STAMP,
        "updated_at": kw.pop("updated_at", STAMP),
        "parent_id": parent_id,
        "persona": None,
        "status": None,
        "busy": False,
        "waiting_for_user": False,
        "report_owed_at": None,
        "archived_at": None,
    }
    out.update(kw)
    return out


def message(id, conv, role, blocks, created_at=STAMP):
    return {
        "id": id,
        "conversation_id": conv,
        "role": role,
        "content": json.dumps(blocks),
        "created_at": created_at,
    }


def choice(provider_id, model, provider_name="prov"):
    return {
        "provider_id": provider_id,
        "provider_name": provider_name,
        "model": model,
        "context_window": 200000,
        "context_window_known": True,
        "tools": None,
    }


def window(name, utilization, status="allowed", resets_at="2026-11-13T14:20:00Z",
           seen_at="2026-11-13T12:02:31.123456Z", reset_passed=False, elapsed_fraction=None, projected=None):
    """One UsageWindow, as src/claude_cli/usage.rs serializes it."""
    return {"name": name, "utilization": utilization, "status": status, "resets_at": resets_at,
            "seen_at": seen_at, "reset_passed": reset_passed, "elapsed_fraction": elapsed_fraction,
            "projected": projected}


def usage(provider_id, windows, observed_at="2026-11-13T12:03:00Z"):
    """A ProviderUsage, GET /api/providers/{id}/usage's answer."""
    return {"provider_id": provider_id, "windows": windows, "observed_at": observed_at}


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.nodes = []
        self.messages = {}
        self.questions = {}
        self.models = {}
        self.providers = [
            {"id": 1, "name": "ClaudeCode", "kind": "claude_cli", "base_url": "/usr/bin/claude",
             "auth_kind": "api_key", "secret_hint": None},
            {"id": 2, "name": "strata", "kind": "llama_cpp", "base_url": "http://strata:8080",
             "auth_kind": "api_key", "secret_hint": "\u2026abcd"},
        ]
        self.pods = []
        self.pod_commands = []
        self.next_id = 100
        self.next_message_id = 1000
        self.ignore_model_set = False
        # (method, path) -> (status, body): answered instead of the route.
        self.forced = {}
        # Answer this many requests with 500 before serving normally.
        self.fail_next = 0
        self.requests = []
        self.build_id = "fakebuild0001"
        # compact (SME-482): what was compacted, with its note, and
        # conversations whose compaction the server refuses (a 409).
        self.compacted = []
        self.compact_refusals = {}
        # A top-level persona (SME-481): what each conversation is set to
        # (its TopLevelPersona), the labels a set accepts, the model a
        # persona pins (label -> (provider_id, model)), conversations whose
        # set the server refuses (a 500, as a ServerFnError), and a set
        # that is kept but not applied.
        self.personas = {}
        self.persona_options = ["smelt:developer", "smelt:project-manager", "smelt:ticket-driver"]
        self.persona_models = {}
        self.persona_refusals = {}
        self.persona_unapplied = False
        # archive/unarchive (SME-480): answer 200 but change nothing.
        self.ignore_archive = False
        # GET /api/providers/{id}/usage (SME-486): provider id -> the
        # ProviderUsage it answers; a provider not listed answers as a
        # missing one does (a ServerFnError, a 500).
        self.usage = {1: usage(1, [])}

    def default_model(self):
        return {"state": "Default", "choice": choice(1, "claude-opus-5-5", "ClaudeCode")}


class Handler(BaseHTTPRequestHandler):
    server_version = "fake-smelt"

    def log_message(self, *args):
        pass

    def _reply(self, status, body):
        raw = json.dumps(body).encode() if not isinstance(body, bytes) else body
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _handle(self, method):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            body = json.loads(raw) if raw else None
        except ValueError:
            body = raw.decode(errors="replace")
        st = self.server.state
        with st.lock:
            st.requests.append({
                "method": method,
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": body,
            })
            if st.fail_next > 0:
                st.fail_next -= 1
                return self._reply(500, {"message": "fake outage"})
            forced = st.forced.get((method, self.path))
            if forced is not None:
                return self._reply(*forced)
            status, out = route(st, method, self.path, body or {})
        self._reply(status, out)

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")

    def do_DELETE(self):
        self._handle("DELETE")

    def do_PUT(self):
        self._handle("PUT")


def route(st, method, path, body):
    conv = re.fullmatch(r"/api/conversations/(\d+)/([a-z/]+)", path)
    if method == "GET" and path == "/api/build-id":
        return 200, st.build_id
    if method == "POST" and path == "/api/conversations":
        cid = st.next_id
        st.next_id += 1
        st.nodes.append(node(cid, title="New conversation"))
        return 200, {"id": cid, "title": "New conversation", "created_at": STAMP,
                     "updated_at": STAMP, "parent_id": None}
    if method == "GET" and path == "/api/conversations/tree":
        return 200, st.nodes
    if method == "GET" and path == "/api/pods":
        return 200, st.pods
    if method == "GET" and path == "/api/pods/commands":
        return 200, st.pod_commands
    if method == "GET" and path == "/api/providers":
        return 200, st.providers
    provider_usage = re.fullmatch(r"/api/providers/(\d+)/usage", path)
    if method == "GET" and provider_usage:
        found = st.usage.get(int(provider_usage.group(1)))
        if found is None:
            return 500, {"message": "That provider no longer exists."}
        return 200, found
    if not conv:
        return 404, {"message": f"no route {method} {path}"}
    cid, rest = int(conv.group(1)), conv.group(2)
    if method == "POST" and rest in ("archive", "unarchive"):
        return set_archived(st, cid, rest == "archive")
    if not any(n["id"] == cid for n in st.nodes):
        return 500, {"message": "That conversation no longer exists."}
    if method == "GET" and rest == "messages":
        return 200, st.messages.get(cid, [])
    if method == "POST" and rest == "messages":
        if not body.get("content"):
            return 500, {"message": "empty message"}
        msgs = st.messages.setdefault(cid, [])
        msgs.append(message(st.next_message_id, cid, "user", [{"type": "text", "text": body["content"]}]))
        st.next_message_id += 1
        return 200, None
    if method == "GET" and rest == "question":
        return 200, st.questions.get(cid)
    if method == "POST" and rest == "question/answer":
        q = st.questions.get(cid)
        if not q or q["tool_use_id"] != body.get("tool_use_id"):
            return 500, {"message": "no such question"}
        st.questions[cid] = None
        return 200, None
    if method == "GET" and rest == "model":
        return 200, st.models.get(cid, st.default_model())
    if method == "POST" and rest == "model":
        prov = [p for p in st.providers if p["id"] == body.get("provider_id")]
        if not prov or not body.get("model"):
            return 500, {"message": "unknown provider or empty model"}
        if not st.ignore_model_set:
            st.models[cid] = {"state": "Chosen", "choice": choice(prov[0]["id"], body["model"], prov[0]["name"])}
        return 200, None
    if method == "POST" and rest == "compact":
        if cid in st.compact_refusals:
            return 409, {"message": st.compact_refusals[cid]}
        st.compacted.append((cid, body.get("note")))
        return 200, None
    if method == "GET" and rest == "persona":
        return 200, st.personas.get(cid) or {
            "set": None, "can_set": None, "notes": [],
            "options": [{"label": o, "description": None, "error": None} for o in st.persona_options]}
    if method == "POST" and rest == "persona":
        return set_persona(st, cid, body.get("persona"))
    if method == "POST" and rest == "repos":
        if not str(body.get("url", "")).startswith(("https://", "git@")):
            return 500, {"message": f"can't clone {body.get('url')!r}"}
        return 200, {"url": body["url"], "branch": body.get("branch", ""), "dir": body.get("dir") or "repo"}
    return 404, {"message": f"no route {method} {path}"}


def set_archived(st, cid, archived):
    """POST …/archive and …/unarchive, as src/api/archive.rs answers: 404
    for a missing id, 409 naming the root for an agent, else the
    Conversation with its mark. The tree reports each node's root's mark,
    so the root's descendants take it too."""
    by_id = {n["id"]: n for n in st.nodes}
    target = by_id.get(cid)
    if target is None:
        # src/api/chat.rs's conversation_not_found.
        return 404, {"message": "conversation not found"}
    if target["parent_id"] is not None:
        root = target
        while root["parent_id"] in by_id:
            root = by_id[root["parent_id"]]
        return 409, {"message": f"only a top-level conversation is archived: archive its root, conversation {root['id']}"}
    if not st.ignore_archive:
        mark = STAMP if archived else None
        tree = {cid}
        for n in st.nodes:  # the tests add parents before their children
            if n["parent_id"] in tree:
                tree.add(n["id"])
        for n in st.nodes:
            if n["id"] in tree:
                n["archived_at"] = mark
    return 200, {"id": cid, "title": target["title"], "created_at": STAMP, "updated_at": STAMP,
                 "parent_id": None, "archived_at": target["archived_at"]}


def set_persona(st, cid, label):
    """POST …/persona: refused as the real set_top_level_persona refuses
    (a ServerFnError is a 500), else the SetPersona it answers."""
    if cid in st.persona_refusals:
        return 500, {"message": st.persona_refusals[cid]}
    current = st.personas.get(cid)
    if current and current["set"] and current["set"]["applied"]:
        return 500, {"message": "this conversation's persona is already set"}
    if label not in st.persona_options:
        return 500, {"message": f"no persona {label!r} in this conversation's checkouts"}
    if label in st.persona_models:
        provider_id, name = st.persona_models[label]
        prov = [p for p in st.providers if p["id"] == provider_id][0]
        st.models[cid] = {"state": "Chosen", "choice": choice(provider_id, name, prov["name"])}
    model = (st.models.get(cid) or st.default_model()).get("choice") or {}
    repo, name = label.split(":")
    out = {"label": label, "path": f"/workspace/{repo}/.agents/personas/{name}.md", "commit": "d687835b",
           "applied": not st.persona_unapplied, "model": model.get("model"), "provider": model.get("provider_name")}
    st.personas[cid] = {"set": out, "can_set": "this conversation's persona is set", "options": [], "notes": []}
    return 200, out


class FakeSmelt:
    def __init__(self):
        self.state = State()
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.state = self.state
        self.port = self.httpd.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def requests(self):
        with self.state.lock:
            return list(self.state.requests)

    def clear(self):
        with self.state.lock:
            self.state.requests.clear()
