"""A minimal Anthropic Messages server for scripts/test-pm-live (SME-460).

  python3 -I mock_upstream.py PORT RULES.json

Every streamed POST /v1/messages is answered with the first unused rule
whose `when` text appears in the request's last message, or, when none
matches, a one-line text reply. A rule is {"when": "...", "tool": NAME,
"id": TOOL_USE_ID, "input": {...}} and answers with that one tool call
(stop_reason tool_use); each rule is used once. A request that doesn't
stream (a title, say) gets a short JSON text message. GET /v1/models lists
one model, `mock-model`. GET /mock/log returns every request's model and
last message, for the harness.

The SSE shape is the browser tier's (src/browser_tests/mock.rs).
"""

import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
LOG = []


def event(name, data):
    return f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()


def text_chunks(text):
    return [
        event("message_start", {"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "content": [],
            "model": "mock-model", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        event("content_block_start", {"type": "content_block_start", "index": 0,
                                      "content_block": {"type": "text", "text": ""}}),
        event("content_block_delta", {"type": "content_block_delta", "index": 0,
                                      "delta": {"type": "text_delta", "text": text}}),
        event("content_block_stop", {"type": "content_block_stop", "index": 0}),
        event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                                "usage": {"output_tokens": 5}}),
        event("message_stop", {"type": "message_stop"}),
    ]


def tool_chunks(name, tool_id, tool_input):
    return [
        event("message_start", {"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "content": [],
            "model": "mock-model", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {
            "type": "tool_use", "id": tool_id, "name": name, "input": {}}}),
        event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {
            "type": "input_json_delta", "partial_json": json.dumps(tool_input)}}),
        event("content_block_stop", {"type": "content_block_stop", "index": 0}),
        event("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                                "usage": {"output_tokens": 5}}),
        event("message_stop", {"type": "message_stop"}),
    ]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, body, kind="application/json"):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            return self.send(200, {"data": [{"id": "mock-model", "type": "model", "display_name": "Mock",
                                             "created_at": "2026-01-01T00:00:00Z"}],
                                   "has_more": False, "first_id": "mock-model", "last_id": "mock-model"})
        if self.path == "/mock/log":
            with LOCK:
                return self.send(200, LOG)
        self.send(404, {"error": "no such route"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            request = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            return self.send(400, {"error": "not JSON"})
        if not self.path.startswith("/v1/messages"):
            return self.send(404, {"error": "no such route"})
        messages = request.get("messages") or []
        last = json.dumps(messages[-1]) if messages else ""
        rule = None
        with LOCK:
            for r in self.server.rules:
                if not r.get("used") and r["when"] in last:
                    r["used"] = True
                    rule = r
                    break
            LOG.append({"model": request.get("model"), "stream": bool(request.get("stream")),
                        "last": last[:2000], "rule": rule and rule["id"]})
        if not request.get("stream"):
            return self.send(200, {"id": "msg_mock", "type": "message", "role": "assistant", "model": "mock-model",
                                   "content": [{"type": "text", "text": "Mock title"}], "stop_reason": "end_turn",
                                   "usage": {"input_tokens": 10, "output_tokens": 2}})
        chunks = (tool_chunks(rule["tool"], rule["id"], rule.get("input", {})) if rule
                  else text_chunks("Mock reply."))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(chunk)
            self.wfile.flush()
        self.close_connection = True


def main():
    port, rules_path = int(sys.argv[1]), sys.argv[2]
    with open(rules_path) as f:
        rules = json.load(f)
    httpd = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    httpd.rules = rules
    print(f"mock upstream on 127.0.0.1:{httpd.server_address[1]}", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
