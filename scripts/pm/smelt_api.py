"""Shared by scripts/pm/smelt-api and scripts/pm/smelt-wait (SME-460).

The address smelt is reached at, one request to it, the route table, and
the tree helpers. The route table is the whole of the API these scripts
use: every request goes through `call`, which takes a route's name, never
a method or a path, so nothing outside the table can be sent. Nothing here
sends DELETE. See docs/personas/pm-in-smelt.md.

No `__main__`: imported by the two scripts, which put this file's
directory on sys.path themselves (they run as `python3 -I`).
"""

import json
import os
import urllib.error
import urllib.parse
import urllib.request

# The in-cluster Service a smelt sandbox reaches (terraform/smelt/server.tf:
# Service `smelt`, port 8080, namespace `smelt-park`).
DEFAULT_URL = "http://smelt.smelt-park.svc.cluster.local:8080"
# The server refuses a Host it doesn't know (src/request_guard.rs), and the
# deployment allows only its public name: sent while SMELT_API_URL is unset.
DEFAULT_HOST = "smelt.constantlee.us"

TIMEOUT_SECS = 30

# name -> (method, path template). `{id}` is a conversation id, an integer.
ROUTES = {
    "create": ("POST", "/api/conversations"),
    "attach": ("POST", "/api/conversations/{id}/repos"),
    "send": ("POST", "/api/conversations/{id}/messages"),
    "model-set": ("POST", "/api/conversations/{id}/model"),
    "answer": ("POST", "/api/conversations/{id}/question/answer"),
    "tree": ("GET", "/api/conversations/tree"),
    "messages": ("GET", "/api/conversations/{id}/messages"),
    "question": ("GET", "/api/conversations/{id}/question"),
    "model": ("GET", "/api/conversations/{id}/model"),
    "pods": ("GET", "/api/pods"),
    "pod-commands": ("GET", "/api/pods/commands"),
    "providers": ("GET", "/api/providers"),
    "build-id": ("GET", "/api/build-id"),
}

ALLOWED_METHODS = ("GET", "POST")

# Subcommands whose route isn't on the server yet: each prints its line and
# exits 3. When its ticket merges, it becomes a ROUTES row instead.
NOT_YET = {
    "archive": "archive arrives with SME-480 (POST /api/conversations/{id}/archive); not on this server yet",
    "unarchive": "unarchive arrives with SME-480 (POST /api/conversations/{id}/unarchive); not on this server yet",
    "compact": "compact arrives with SME-482 (POST /api/conversations/{id}/compact); not on this server yet",
}


class NetworkError(Exception):
    """smelt couldn't be reached at all (no HTTP status)."""


class AddressError(ValueError):
    """SMELT_API_URL isn't scheme://host[:port]. Its text never holds the
    URL, which could carry a password."""


URL_RULE = ("SMELT_API_URL must be http(s)://host[:port] and nothing more "
            "(no user, path, query or fragment); nothing sent")


def checked_url(url):
    """`url` without a trailing slash, or AddressError. Anything after the
    host and port would swallow the route table's path (L1)."""
    try:
        parts = urllib.parse.urlsplit(url)
        parts.port  # raises ValueError for a port out of range
    except ValueError:
        raise AddressError(URL_RULE) from None
    if (parts.scheme.lower() not in ("http", "https") or not parts.hostname or "@" in parts.netloc
            or parts.path not in ("", "/") or "?" in url or "#" in url or any(c.isspace() for c in url)):
        raise AddressError(URL_RULE)
    return url.rstrip("/")


def address(env=None):
    """(base URL, Host header or None for the URL's own); AddressError for
    a SMELT_API_URL that isn't scheme://host[:port]."""
    env = os.environ if env is None else env
    url = env.get("SMELT_API_URL", "")
    if url:
        base = checked_url(url)
        host = env.get("SMELT_API_HOST") or None
    else:
        base = DEFAULT_URL
        host = env.get("SMELT_API_HOST", DEFAULT_HOST) or None
    return base, host


def sent_host(base, host):
    """The Host value a request to `base` carries."""
    return host if host else urllib.parse.urlsplit(base).netloc


def conv_id(value):
    """A conversation id from the command line: digits only, else ValueError."""
    text = str(value)
    if not text.isdigit():
        raise ValueError(f"not a conversation id: {text!r}")
    return int(text)


def path_for(route, conv=None):
    method, template = ROUTES[route]
    if method not in ALLOWED_METHODS:
        raise ValueError(f"method {method} isn't allowed")
    if "{id}" in template:
        if conv is None:
            raise ValueError(f"route {route} needs a conversation id")
        template = template.replace("{id}", str(conv_id(conv)))
    return method, template


def call(route, conv=None, body=None, env=None):
    """One request: (HTTP status, parsed JSON body, or its text if not JSON).

    Raises NetworkError when nothing answered.
    """
    method, path = path_for(route, conv)
    base, host = address(env)
    data = None
    if method == "POST":
        data = json.dumps(body if body is not None else {}).encode()
    req = urllib.request.Request(base + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    req.add_header("Accept", "application/json")
    if host:
        req.add_header("Host", host)
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT_SECS) as resp:
            status, raw = resp.status, resp.read()
    except urllib.error.HTTPError as err:
        status, raw = err.code, err.read()
    except (urllib.error.URLError, OSError) as err:
        raise NetworkError(f"{base}{path}: {err}") from err
    text = raw.decode("utf-8", errors="replace")
    try:
        return status, json.loads(text) if text else None
    except ValueError:
        return status, text


def ok(status):
    return 200 <= status < 300


def subtree(nodes, root):
    """ROOT's node and its descendants at any depth, as (depth, node) in
    depth-first order; [] when ROOT isn't in `nodes`."""
    by_parent = {}
    by_id = {}
    for node in nodes:
        by_id[node["id"]] = node
        by_parent.setdefault(node.get("parent_id"), []).append(node)
    if root not in by_id:
        return []
    out = []
    stack = [(0, by_id[root])]
    seen = set()
    while stack:
        depth, node = stack.pop()
        if node["id"] in seen:
            continue
        seen.add(node["id"])
        out.append((depth, node))
        children = sorted(by_parent.get(node["id"], []), key=lambda n: n["id"])
        for child in reversed(children):
            stack.append((depth + 1, child))
    return out


def message_blocks(message):
    """A message's content blocks: `content` is a JSON string of blocks (or,
    from an older row, plain text)."""
    content = message.get("content")
    if isinstance(content, str):
        try:
            content = json.loads(content)
        except ValueError:
            return [{"type": "text", "text": content}]
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if isinstance(content, dict):
        return [content]
    return content or []
