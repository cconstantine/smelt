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
import math
import os
import re
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

# name -> (method, path template). `{id}` is a conversation or (for
# `usage`) a provider id, an integer either way.
ROUTES = {
    "create": ("POST", "/api/conversations"),
    "attach": ("POST", "/api/conversations/{id}/repos"),
    "send": ("POST", "/api/conversations/{id}/messages"),
    "model-set": ("POST", "/api/conversations/{id}/model"),
    "answer": ("POST", "/api/conversations/{id}/question/answer"),
    "compact": ("POST", "/api/conversations/{id}/compact"),
    "tree": ("GET", "/api/conversations/tree"),
    "messages": ("GET", "/api/conversations/{id}/messages"),
    "question": ("GET", "/api/conversations/{id}/question"),
    "model": ("GET", "/api/conversations/{id}/model"),
    "persona-set": ("POST", "/api/conversations/{id}/persona"),
    "persona": ("GET", "/api/conversations/{id}/persona"),
    "pods": ("GET", "/api/pods"),
    "pod-commands": ("GET", "/api/pods/commands"),
    "providers": ("GET", "/api/providers"),
    # SME-486: `{id}` is a provider's id here.
    "usage": ("GET", "/api/providers/{id}/usage"),
    "build-id": ("GET", "/api/build-id"),
    # SME-480: a view mark on a top-level conversation and its agents.
    "archive": ("POST", "/api/conversations/{id}/archive"),
    "unarchive": ("POST", "/api/conversations/{id}/unarchive"),
}

ALLOWED_METHODS = ("GET", "POST")

# Subcommands whose route isn't on the server yet: each prints its line and
# exits 3. When its ticket merges, it becomes a ROUTES row instead (as
# archive and unarchive did when SME-480 merged). Empty today.
NOT_YET = {}


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


# The usage hold (SME-486, docs/personas/project-manager.md): a live
# window at or over these holds new Drivers. Whole percents: the verdict
# compares the percent it prints (round 1 B-L3). The five-hour window's
# percent isn't one of them: smelt's own gate holds every Claude Code
# call at 95% of it (SME-523 Q7); its status still holds.
HOLD_SEVEN_DAY = 90
HOLD_PACE = 100
HOLD_STATUSES = ("allowed_warning", "rejected")

# The shape a window's name or status must have to print as itself: the
# answer's other fields, and any text that isn't a plain word, never print.
WINDOW_WORD = re.compile(r"[a-z0-9_]{1,40}")
TIME = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d(:\d\d(\.\d+)?)?(Z|\+00:00)")


def _number(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _time(value):
    """An RFC 3339 UTC time cut to the minute (`2026-11-13T14:20Z`), or None
    for anything that isn't one."""
    return value[:16] + "Z" if isinstance(value, str) and TIME.fullmatch(value) else None


def _word(value, otherwise):
    return value if isinstance(value, str) and WINDOW_WORD.fullmatch(value) else otherwise


NO_USAGE_ROUTE = "unknown: no usage route"


def is_usage_answer(answer):
    """Whether `answer` is a ProviderUsage: a dict with a windows list. A
    server before SME-486 answers the route 200 with its NotFound page."""
    return isinstance(answer, dict) and isinstance(answer.get("windows"), list)


def usage_windows(answer):
    """The whitelisted fields of each window in a ProviderUsage answer
    (SME-486): numbers, booleans, minute times, and a name or status
    that is a plain word (else `(unnamed)` / `(unknown)`). Nothing else
    of the answer is kept. Only seven_day keeps its pace (`projected`):
    it's the only pace the verdict and the UI use."""
    windows = answer.get("windows") if isinstance(answer, dict) else None
    out = []
    for w in windows if isinstance(windows, list) else []:
        if not isinstance(w, dict):
            continue
        name = _word(w.get("name"), "(unnamed)")
        out.append({
            "name": name,
            "utilization": _number(w.get("utilization")) or 0.0,
            "status": _word(w.get("status"), "(unknown)"),
            "resets_at": _time(w.get("resets_at")),
            "seen_at": _time(w.get("seen_at")),
            "reset_passed": w.get("reset_passed") is True,
            "projected": _number(w.get("projected")) if name == "seven_day" else None,
        })
    return out


def whole_percent(fraction):
    """`fraction` as a whole percent, a half rounding up: Rust's f64::round
    on the provider page and the context bar, not Python's round, which
    rounds a half to even. The verdict compares this, the number printed."""
    value = fraction * 100
    whole = math.floor(value)
    return whole + 1 if value - whole >= 0.5 else whole


def percent(fraction):
    return f"{whole_percent(fraction)}%"


def usage_verdict(windows):
    """`ok`, `hold: <reasons>` or `unknown: no reading`, for windows from
    usage_windows. A window whose reset has passed never holds."""
    live = [w for w in windows if not w["reset_passed"]]
    if not live:
        return "unknown: no reading"
    reasons = []
    for w in live:
        name, used, pace = w["name"], w["utilization"], w["projected"]
        if name == "seven_day" and whole_percent(used) >= HOLD_SEVEN_DAY:
            reasons.append(f"{name} at {percent(used)}")
        elif name == "seven_day" and pace is not None and whole_percent(pace) >= HOLD_PACE:
            reasons.append(f"{name} on pace for {percent(pace)} by reset")
        if w["status"] in HOLD_STATUSES:
            reasons.append(f"{name} {w['status']}")
    return "hold: " + "; ".join(reasons) if reasons else "ok"


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


def _tools_summary(tools):
    if not isinstance(tools, dict):
        return "-"
    parts = [f"{key} {len(tools[key])}" for key in ("allow", "deny") if isinstance(tools.get(key), list)]
    if tools.get("own") is not None:
        parts.append(f"own {json.dumps(tools['own'])}")
    return ", ".join(parts) or "-"


def persona_settings_lines(chosen):
    """A set persona's snapshot settings (SME-516), one `key value` line
    each, `-` for unset. Credentials are names only: the settings never
    hold a value."""
    settings = chosen.get("settings") if isinstance(chosen.get("settings"), dict) else {}
    compact_at = settings.get("compact_at")
    resources = settings.get("resources")
    names = settings.get("credentials") or []
    ignored = settings.get("ignored") or []
    lines = [
        f"compact_at {compact_at if compact_at is not None else '-'}",
        f"resources {json.dumps(resources) if resources else '-'}",
        f"credentials {', '.join(map(str, names)) or '-'}",
        f"tools {_tools_summary(settings.get('tools'))}",
        f"ignored {', '.join(map(str, ignored)) or '-'}",
    ]
    refresh = chosen.get("refresh")
    if isinstance(refresh, dict):
        commit = refresh.get("commit") or ""
        line = f"refresh {refresh.get('state')}" + (f" at {commit[:7]}" if commit else "")
        lines.append(line + (f": {refresh['why']}" if refresh.get("why") else ""))
    return lines
