"""An HTTP CONNECT proxy that refuses chosen connections with a 503 (SME-442).

scripts/repro-cargo-net-flake points cargo at it (CARGO_HTTP_PROXY) to give
a build the network flake a seat's gate met on crates.io, on demand. Every
CONNECT is logged, one line each, as it is decided:

    #3 static.crates.io:443 ok
    #4 static.crates.io:443 REFUSED armed

`armed` marks a CONNECT made once the arm file existed, so "how many
connections after arming" is a count of the lines carrying it.

    python3 -I flaky-connect-proxy.py --log LOG --ready FILE
        [--refuse-first N | --refuse-all]
        [--arm-file F --armed-refuse N|all]

--refuse-first N refuses the first N CONNECTs (from the start);
--refuse-all refuses every one. --arm-file F --armed-refuse N refuses the
next N (or all) CONNECTs once F exists. Without a refusal option every
CONNECT is let through. --ready FILE gets the listening port once bound
(on 127.0.0.1, a port the kernel picks). Ends itself after --lifetime
seconds (3600 by default), so a run killed before its cleanup leaves
nothing behind for long. Standard library only.
"""
import argparse
import os
import select
import signal
import socket
import socketserver
import threading

parser = argparse.ArgumentParser()
parser.add_argument("--log", required=True)
parser.add_argument("--ready", required=True)
parser.add_argument("--refuse-first", type=int, default=0)
parser.add_argument("--refuse-all", action="store_true")
parser.add_argument("--arm-file")
parser.add_argument("--armed-refuse", default="0")
parser.add_argument("--lifetime", type=int, default=3600)
args = parser.parse_args()
armed_refuse = None if args.armed_refuse == "all" else int(args.armed_refuse)

lock = threading.Lock()
state = {"n": 0, "armed_n": 0}
log = open(args.log, "a", buffering=1)


def decide(target):
    """Numbers this CONNECT, logs it, and says whether to refuse it."""
    with lock:
        state["n"] += 1
        n = state["n"]
        armed = bool(args.arm_file) and os.path.exists(args.arm_file)
        refuse = args.refuse_all or n <= args.refuse_first
        if armed:
            state["armed_n"] += 1
            if armed_refuse is None or state["armed_n"] <= armed_refuse:
                refuse = True
        log.write(f"#{n} {target} {'REFUSED' if refuse else 'ok'}{' armed' if armed else ''}\n")
        return refuse


def pipe(a, b):
    """Copies bytes both ways until either side closes."""
    socks = [a, b]
    while True:
        readable, _, _ = select.select(socks, [], [], 300)
        if not readable:
            return
        for s in readable:
            data = s.recv(65536)
            if not data:
                return
            (b if s is a else a).sendall(data)


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        conn = self.request
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = conn.recv(4096)
            if not chunk:
                return
            head += chunk
            if len(head) > 65536:
                return
        line = head.split(b"\r\n", 1)[0].decode("latin-1")
        parts = line.split()
        if len(parts) < 2 or parts[0] != "CONNECT":
            log.write(f"not a CONNECT: {line}\n")
            conn.sendall(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            return
        target = parts[1]
        if decide(target):
            conn.sendall(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            return
        host, _, port = target.rpartition(":")
        try:
            upstream = socket.create_connection((host, int(port)), timeout=30)
        except OSError as e:
            log.write(f"   {target} upstream failed: {e}\n")
            conn.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            return
        upstream.settimeout(None)
        with upstream:
            conn.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
            rest = head.split(b"\r\n\r\n", 1)[1]
            if rest:
                upstream.sendall(rest)
            pipe(conn, upstream)


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


signal.alarm(args.lifetime)
server = Server(("127.0.0.1", 0), Handler)
with open(args.ready + ".tmp", "w") as ready:
    ready.write(str(server.server_address[1]))
os.rename(args.ready + ".tmp", args.ready)
server.serve_forever()
