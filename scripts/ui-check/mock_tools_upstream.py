"""A stand-in Anthropic API for hands-on checks that need tool calls but not a
real model (SME-144). Run: `python3 -I mock_tools_upstream.py <port>`. It
lists one model, `mock-tools`.
A user message `TOOL <name> <json input>` makes the turn call that tool once,
then answer "done." once the result comes back. Anything else (a command's
finished notice waking the model) gets "noted." with no tool call."""
import json, sys, time, itertools
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ids = itertools.count(1)

def sse(name, data):
    return f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()

def text_of(content):
    if isinstance(content, str):
        return content
    return "\n".join(b.get("text", "") for b in content if b.get("type") == "text")

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            body = json.dumps({"data": [{"id": "mock-tools", "display_name": "Mock tools model",
                                         "max_input_tokens": 200000}], "has_more": False}).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.send_header("content-length", "0"); self.end_headers()

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
        last = body["messages"][-1]["content"]
        answered = isinstance(last, list) and any(b.get("type") == "tool_result" for b in last)
        ask = text_of(last).strip()
        call = None
        if not answered and ask.startswith("TOOL "):
            _, name, raw = ask.split(" ", 2)
            call = (name, raw)
        self.send_response(200); self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked"); self.end_headers()
        def send(name, data):
            chunk = sse(name, data)
            self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n"); self.wfile.flush()
            time.sleep(0.05)
        send("message_start", {"type": "message_start", "message": {"id": "msg_mock", "usage": {"input_tokens": 50, "output_tokens": 1}}})
        send("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        say = f"Calling {call[0]}." if call else ("done." if answered else "noted.")
        send("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": say}})
        send("content_block_stop", {"type": "content_block_stop", "index": 0})
        stop = "end_turn"
        if call:
            send("content_block_start", {"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": f"toolu_mock_{next(ids)}_{int(time.time())}", "name": call[0], "input": {}}})
            send("content_block_delta", {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": call[1]}})
            send("content_block_stop", {"type": "content_block_stop", "index": 1})
            stop = "tool_use"
        send("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 40}})
        send("message_stop", {"type": "message_stop"})
        self.wfile.write(b"0\r\n\r\n"); self.wfile.flush()

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
