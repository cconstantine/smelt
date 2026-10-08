"""Helpers for hands-on checks of smelt through its web UI, with Playwright.

Written for bug bashes (SME-51): drive the real app, with a real model, the
way a user would. Run a check server first (`scripts/check-server start`),
then a short script of your own:

    /opt/playwright-venv/bin/python my_check.py

    import sys; sys.path.insert(0, "scripts/ui-check")
    from smelt_ui import run, log

    def check(tab, ctx):
        t = tab()
        cid = t.new_conversation()
        t.send("In one short line: what is today's date?")
        t.wait_idle()
        log(t.last_messages(1))

    run(check)

`run` deletes every conversation its tabs started, in a `finally`, even when
the check fails or times out: a conversation's sandbox pod and claims carry
the check server's scratch-database instance, so once `check-server stop`
drops that database no server ever deletes them (SME-126 left one behind
this way). Prefer asks that don't start a sandbox unless the check is about
one, and give a local model time: `wait_idle` waits 15 minutes by default.

Things that cost a rerun on SME-51, handled here:
- A conversation page keeps an event stream open, so waiting for "network
  idle" never ends: pages wait for `load`, then for hydration.
- The message box is an `<input>` in `.composer`, not a textarea.
- dx's dev overlay ("Your app is being rebuilt") is in the DOM even when
  hidden; read specific elements, not the whole body.
- While a reply streams, its last word may be half-written.

Screenshots go to `SMELT_UI_OUT` (default: `./ui-check-out`). `SMELT_UI_BASE`
is the app's address (default: the check server, http://localhost:8081).
"""
import os
import re
import sys
import time

from playwright.sync_api import sync_playwright

BASE = os.environ.get("SMELT_UI_BASE", "http://localhost:8081").rstrip("/")
OUT = os.environ.get("SMELT_UI_OUT", os.path.join(os.getcwd(), "ui-check-out"))


def log(*parts):
    print(time.strftime("%H:%M:%S"), *parts, flush=True)


class Tab:
    """One browser tab on the app."""

    def __init__(self, page, started=None):
        self.page = page
        # Conversation ids this check started, shared by every tab of one
        # `run`, which deletes them at the end.
        self.started = started if started is not None else []

    # --- Navigation ---

    def goto(self, path="/"):
        self.page.goto(BASE + path, wait_until="load")
        self.page.wait_for_timeout(3000)  # hydration
        return self

    def new_conversation(self):
        """Starts a conversation from the sidebar and returns its id."""
        self.goto("/")
        self.page.click(".new-conversation")
        self.page.wait_for_url(re.compile(r".*/conversation/\d+"), timeout=15000)
        self.page.wait_for_timeout(1000)
        conversation_id = self.conversation_id()
        if conversation_id is not None:
            self.started.append(conversation_id)
        return conversation_id

    def conversation_id(self):
        match = re.search(r"/conversation/(\d+)", self.page.url)
        return int(match.group(1)) if match else None

    def delete_conversation(self, conversation_id):
        """The sidebar's two-click Delete. Deleting also removes its pod."""
        row = self.page.locator(f'[data-conversation-id="{conversation_id}"]')
        if not row.count():
            return False
        button = row.locator(".delete-conversation")
        button.click()
        self.page.wait_for_timeout(300)
        button.click()
        self.page.wait_for_timeout(2500)
        if conversation_id in self.started:
            self.started.remove(conversation_id)
        return True

    # --- Turns ---

    def send(self, text):
        box = self.page.locator('.composer input[placeholder="Type a message..."]')
        box.fill(text)
        box.press("Enter")
        log("sent:", text[:80])

    def working(self):
        return self.page.locator(".turn-working").count() > 0

    def wait_idle(self, timeout=900, start_timeout=20):
        """Waits for a turn to start (briefly), then to finish. A local model
        can take minutes per turn. Returns False if it's still running."""
        started = time.time()
        while time.time() - started < start_timeout and not self.working():
            self.page.wait_for_timeout(500)
        while self.working():
            if time.time() - started > timeout:
                log(f"still working after {timeout}s")
                return False
            self.page.wait_for_timeout(2000)
        log("idle after %.0fs" % (time.time() - started))
        return True

    def stop(self):
        self.page.click(".stop-turn")

    def streaming(self):
        """The reply streaming in right now, or ''. Its last word may be
        half-written."""
        loc = self.page.locator(".message-streaming")
        return loc.first.inner_text() if loc.count() else ""

    # --- Reading the page ---

    def text(self, selector):
        loc = self.page.locator(selector)
        return "\n---\n".join(loc.nth(i).inner_text() for i in range(loc.count()))

    def transcript(self):
        return self.text(".messages")

    def last_messages(self, n=3):
        """The last `n` messages and notices, in order."""
        loc = self.page.locator(".messages .message, .messages .system-notice")
        count = loc.count()
        return "\n---\n".join(loc.nth(i).inner_text() for i in range(max(0, count - n), count))

    def notices(self):
        loc = self.page.locator(".system-notice")
        return [loc.nth(i).inner_text() for i in range(loc.count())]

    def tool_rows(self):
        loc = self.page.locator(".tool-row-summary")
        return [loc.nth(i).inner_text() for i in range(loc.count())]

    def shot(self, name):
        os.makedirs(OUT, exist_ok=True)
        path = os.path.join(OUT, name + ".png")
        self.page.screenshot(path=path)
        log("screenshot", path)
        return path


def run(check, width=1400, height=900, color_scheme="light"):
    """Runs `check(tab, context)` in a fresh browser. `tab()` opens a new tab
    in the same browser context, for two-tab checks. Every conversation a tab
    started with `new_conversation` and didn't delete is deleted at the end,
    even if the check raised. Console errors are printed at the end."""
    with sync_playwright() as p:
        browser = p.chromium.launch()
        context = browser.new_context(viewport={"width": width, "height": height}, color_scheme=color_scheme)
        errors = []
        started = []

        def tab():
            page = context.new_page()
            page.on("console", lambda m: errors.append(f"console: {m.text}") if m.type == "error" else None)
            page.on("pageerror", lambda e: errors.append(f"page error: {e}"))
            return Tab(page, started)

        try:
            check(tab, context)
        finally:
            _delete_started(tab, started)
            if errors:
                log("browser errors:\n  " + "\n  ".join(errors[:20]))
            browser.close()


def _delete_started(tab, started):
    """Deletes the conversations a check left behind, from a fresh tab."""
    if not started:
        return
    try:
        cleaner = tab().goto("/")
    except Exception as e:  # noqa: BLE001 - report and carry on to close the browser
        log(f"cleanup: couldn't open a tab to delete conversations {started}: {e}")
        return
    for conversation_id in list(started):
        try:
            if cleaner.delete_conversation(conversation_id):
                log(f"cleanup: deleted conversation {conversation_id}")
            else:
                log(f"cleanup: conversation {conversation_id} not in the sidebar; delete it by hand")
        except Exception as e:  # noqa: BLE001
            log(f"cleanup: deleting conversation {conversation_id} failed: {e}")


if __name__ == "__main__":
    print(__doc__, file=sys.stderr)
