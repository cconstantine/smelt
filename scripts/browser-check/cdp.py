"""Minimal raw WebSocket + Chrome DevTools Protocol client, pure stdlib.

No `websockets`/`websocket-client` package and no Node are assumed to be
available (neither was, the day this was written) — this hand-rolls just
enough RFC6455 framing (client->server frames must be masked; this only
ever needs to read single-frame, unmasked, text-opcode server->client
frames, which is all CDP ever sends) to open one WebSocket and speak CDP's
JSON-RPC-over-WS shape. Not a general WebSocket client — no fragmentation,
no ping/pong replies, no binary frames.
"""

import base64
import json
import os
import socket
import struct
import urllib.request


class CDPConnection:
    def __init__(self, ws_url):
        assert ws_url.startswith("ws://"), ws_url
        rest = ws_url[len("ws://") :]
        hostport, _, path = rest.partition("/")
        path = "/" + path
        host, _, port = hostport.partition(":")
        port = int(port or 80)

        self.sock = socket.create_connection((host, port), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {hostport}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            resp += self.sock.recv(4096)
        assert b" 101 " in resp.split(b"\r\n", 1)[0], resp[:200]
        self._id = 0
        self._buf = b""

    def _send_frame(self, data: bytes):
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        length = len(data)
        header = bytearray([0x81])  # FIN + text opcode
        if length <= 125:
            header.append(0x80 | length)
        elif length <= 0xFFFF:
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        header += mask
        self.sock.sendall(bytes(header) + masked)

    def _recv_exact(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("socket closed")
            self._buf += chunk
        data, self._buf = self._buf[:n], self._buf[n:]
        return data

    def _recv_frame(self):
        header = self._recv_exact(2)
        b0, b1 = header[0], header[1]
        opcode = b0 & 0x0F
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack(">H", self._recv_exact(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self._recv_exact(8))[0]
        payload = self._recv_exact(length)
        return opcode, payload

    def recv_json(self):
        while True:
            opcode, payload = self._recv_frame()
            if opcode == 0x1:  # text frame
                return json.loads(payload.decode())
            if opcode == 0x8:  # close
                raise ConnectionError("closed by server")
            # ignore ping/pong/binary — not needed for CDP's usage

    def send(self, method, params=None, session_id=None):
        self._id += 1
        msg = {"id": self._id, "method": method, "params": params or {}}
        if session_id:
            msg["sessionId"] = session_id
        self._send_frame(json.dumps(msg).encode())
        return self._id

    def call(self, method, params=None, session_id=None, max_messages=200):
        """Send a command and block for its matching reply, dropping any
        events or other calls' replies received in between. Fine for this
        script's simple sequential-call usage; not safe if you need to
        observe events interleaved with calls."""
        want_id = self.send(method, params, session_id)
        for _ in range(max_messages):
            msg = self.recv_json()
            if msg.get("id") == want_id:
                if "error" in msg:
                    raise RuntimeError(f"{method} failed: {msg['error']}")
                return msg.get("result", {})
        raise TimeoutError(f"no reply to {method}")


def get_browser_ws_url(port=9222):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version") as r:
        return json.load(r)["webSocketDebuggerUrl"]


def new_page_session(port=9222):
    """Creates a new browser tab and returns (connection, session_id) —
    CDP's "flattened session" model, where page-target commands are sent
    over the same browser-level socket tagged with a sessionId, rather than
    opening a second socket to a per-target debugger URL."""
    conn = CDPConnection(get_browser_ws_url(port))
    target = conn.call("Target.createTarget", {"url": "about:blank"})
    attach = conn.call(
        "Target.attachToTarget", {"targetId": target["targetId"], "flatten": True}
    )
    session_id = attach["sessionId"]
    conn.call("Page.enable", session_id=session_id)
    conn.call("Runtime.enable", session_id=session_id)
    return conn, session_id
