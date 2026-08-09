#!/usr/bin/env python3
"""Drive headless Chrome against a running dev server and take a
screenshot — for visually checking UI changes when no browser
extension/Playwright/Node is available. See
docs/testing.md#browser-verification for the full story and setup.sh for
the one-time download this depends on.

Example:
    scripts/browser-check/setup.sh   # once, idempotent
    python3 scripts/browser-check/browser_check.py http://127.0.0.1:8080/ \\
        --screenshot /tmp/out.png \\
        --action "click:.conversation-item" \\
        --action "sleep:1000" \\
        --action "scroll:.messages"

Actions run in the order given on the command line:
    click:SELECTOR       dispatch a click MouseEvent on the first match
    type:SELECTOR=TEXT   set .value and dispatch an input event
    wait:SELECTOR        poll (up to 10s) until the selector exists
    scroll:SELECTOR      set scrollTop = scrollHeight on the element
    sleep:MS             wait MS milliseconds
    eval:JS              evaluate arbitrary JS (escape hatch for anything
                          else — reading text content, injecting markup to
                          preview CSS for a state you don't have live data
                          for, etc.)
"""
import argparse
import base64
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cdp import new_page_session  # noqa: E402

DEFAULT_CACHE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".browser-check-cache")


def find_chrome_binary(cache_dir):
    bin_path = os.path.join(cache_dir, "chrome", "chrome-headless-shell-linux64", "chrome-headless-shell")
    if not os.path.isfile(bin_path):
        sys.exit(f"chrome-headless-shell not found at {bin_path} -- run scripts/browser-check/setup.sh first")
    return bin_path


def launch_chrome(binary, libdir, port, profile_dir):
    env = dict(os.environ)
    env["LD_LIBRARY_PATH"] = f"{libdir}:{libdir}/dri:" + env.get("LD_LIBRARY_PATH", "")
    os.makedirs(profile_dir, exist_ok=True)
    proc = subprocess.Popen(
        [
            binary,
            "--headless",
            "--disable-gpu",
            "--no-sandbox",
            f"--remote-debugging-port={port}",
            "--remote-debugging-address=127.0.0.1",
            f"--user-data-dir={profile_dir}",
            "--window-size=1400,900",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    for _ in range(50):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=0.5):
                return proc
        except Exception:
            time.sleep(0.2)
    proc.kill()
    sys.exit("chrome-headless-shell did not come up within 10s")


def run_action(conn, session, action):
    kind, _, rest = action.partition(":")
    if kind == "click":
        js = (
            f"(() => {{ const el = document.querySelector({rest!r}); "
            "if (!el) return 'not found'; "
            "el.dispatchEvent(new MouseEvent('click', {bubbles: true})); "
            "return 'ok'; })()"
        )
        r = conn.call("Runtime.evaluate", {"expression": js}, session_id=session)
    elif kind == "type":
        selector, _, text = rest.partition("=")
        js = f"""(() => {{
            const el = document.querySelector({selector!r});
            if (!el) return 'not found';
            const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
            setter.call(el, {text!r});
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            return 'ok';
        }})()"""
        r = conn.call("Runtime.evaluate", {"expression": js}, session_id=session)
    elif kind == "wait":
        deadline = time.time() + 10
        while time.time() < deadline:
            r = conn.call(
                "Runtime.evaluate", {"expression": f"!!document.querySelector({rest!r})"}, session_id=session
            )
            if r.get("result", {}).get("value"):
                print(f"{action} -> ok", file=sys.stderr)
                return
            time.sleep(0.2)
        print(f"{action} -> TIMED OUT", file=sys.stderr)
        return
    elif kind == "scroll":
        js = (
            f"(() => {{ const el = document.querySelector({rest!r}); "
            "if (!el) return 'not found'; "
            "el.scrollTop = el.scrollHeight; return 'ok'; })()"
        )
        r = conn.call("Runtime.evaluate", {"expression": js}, session_id=session)
    elif kind == "sleep":
        time.sleep(int(rest) / 1000)
        return
    elif kind == "eval":
        r = conn.call("Runtime.evaluate", {"expression": rest}, session_id=session)
    else:
        sys.exit(f"unknown action kind: {kind!r} (expected click/type/wait/scroll/sleep/eval)")
    print(f"{action} -> {r.get('result', {}).get('value')}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("url")
    parser.add_argument("--screenshot", required=True, help="output PNG path")
    parser.add_argument("--action", action="append", default=[], dest="actions")
    parser.add_argument("--width", type=int, default=1400)
    parser.add_argument("--height", type=int, default=900)
    parser.add_argument("--port", type=int, default=9222)
    parser.add_argument("--cache-dir", default=os.environ.get("BROWSER_CHECK_CACHE", DEFAULT_CACHE_DIR))
    parser.add_argument(
        "--keep-open",
        action="store_true",
        help="leave chrome running after the screenshot (e.g. for several quick runs in a row against --port); "
        "otherwise it's killed on exit so nothing leaks",
    )
    args = parser.parse_args()

    cache_dir = os.path.abspath(args.cache_dir)
    binary = find_chrome_binary(cache_dir)
    libdir = os.path.join(cache_dir, "libs", "usr", "lib", "x86_64-linux-gnu")
    profile_dir = os.path.join(cache_dir, "profile")

    # Reuse an already-listening instance on this port (e.g. from a prior
    # --keep-open run); otherwise launch our own and make sure it's killed
    # when we're done, so repeated runs don't leak orphaned processes.
    proc = None
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{args.port}/json/version", timeout=0.5)
    except Exception:
        proc = launch_chrome(binary, libdir, args.port, profile_dir)

    try:
        conn, session = new_page_session(args.port)
        conn.call(
            "Emulation.setDeviceMetricsOverride",
            {"width": args.width, "height": args.height, "deviceScaleFactor": 1, "mobile": False},
            session_id=session,
        )
        conn.call("Page.navigate", {"url": args.url}, session_id=session)

        for _ in range(100):
            r = conn.call("Runtime.evaluate", {"expression": "document.readyState"}, session_id=session)
            if r.get("result", {}).get("value") == "complete":
                break
            time.sleep(0.1)
        time.sleep(1.0)  # settle time for WASM hydration after the initial parse

        for action in args.actions:
            run_action(conn, session, action)

        shot = conn.call("Page.captureScreenshot", {"format": "png"}, session_id=session)
        with open(args.screenshot, "wb") as f:
            f.write(base64.b64decode(shot["data"]))
        print(f"wrote {args.screenshot}", file=sys.stderr)
    finally:
        if proc is not None and not args.keep_open:
            proc.kill()
            proc.wait(timeout=5)


if __name__ == "__main__":
    main()
