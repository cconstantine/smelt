"""A stub release server for scripts/test-ci-install-compose (SME-458).

    python3 -I flaky-http-server.py --ready FILE --log LOG --file F MODE [N]

Serves F on every GET, except as MODE says:

    ok            serve F
    fail-first N  answer 504 to the first N GETs, then serve F
    always-504    answer 504 to every GET
    hang          accept the connection and never answer
    bad-bytes     answer 200 with bytes that aren't F's

Every GET is logged, one line each, as it is decided: `#n GET <path> -> <status>`
(`hang` for a hung one). --ready FILE gets the listening port once bound (on
127.0.0.1, a port the kernel picks). Ends itself after --lifetime seconds (600
by default), so a run killed before its cleanup leaves nothing behind for long.
Standard library only.
"""
import argparse
import http.server
import os
import signal
import socketserver
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--ready", required=True)
parser.add_argument("--log", required=True)
parser.add_argument("--file", required=True)
parser.add_argument("--lifetime", type=int, default=600)
parser.add_argument("mode", choices=["ok", "fail-first", "always-504", "hang", "bad-bytes"])
parser.add_argument("n", nargs="?", type=int, default=0)
args = parser.parse_args()

lock = threading.Lock()
state = {"n": 0}
log = open(args.log, "a", buffering=1)


def decide():
    """Numbers this GET and says what it gets: a status, or None to hang."""
    with lock:
        state["n"] += 1
        n = state["n"]
    if args.mode == "always-504" or (args.mode == "fail-first" and n <= args.n):
        return n, 504
    if args.mode == "hang":
        return n, None
    return n, 200


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        n, status = decide()
        log.write(f"#{n} GET {self.path} -> {status if status else 'hang'}\n")
        if status is None:
            time.sleep(args.lifetime)
            return
        if status != 200:
            self.send_error(status, "Gateway Time-out")
            return
        if args.mode == "bad-bytes":
            body = b"#!/bin/sh\necho 'not the pinned compose'\n"
        else:
            with open(args.file, "rb") as f:
                body = f.read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


server = Server(("127.0.0.1", 0), Handler)
signal.signal(signal.SIGTERM, lambda *_: os._exit(0))
threading.Timer(args.lifetime, lambda: os._exit(0)).start()
tmp = args.ready + ".tmp"
with open(tmp, "w") as f:
    f.write(str(server.server_address[1]))
os.rename(tmp, args.ready)
server.serve_forever()
