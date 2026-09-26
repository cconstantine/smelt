# Fewer connections per tab, and HTTP/2 in dev

**Branch:** `connection-limits`, stacked on `pod-management` (PR #28, not yet merged: this reworks the chat streaming path that PR changed).

## What

Over plain HTTP/1.1, a browser allows 6 connections per host, shared by every tab (MDN: SSE "when not used over HTTP/2" is limited to 6 per browser and domain; Chrome and Firefox marked it "Won't fix"). A smelt chat tab holds:
- 1 always-open conversation event stream;
- +1 while a reply streams (`send_message` is its own stream);
- +1 while the browsing panel is open (its frames).

Past 6, a new tab never finishes loading, or a click (Stop, a snapshot) waits for a reply to finish. Both happened while building `pod-management`'s browser tests (see its completed doc).

**Production** is served through homelab's TLS front end for `*.constantlee.us`. A probe of `https://smelt.constantlee.us` on 2026-09-26 negotiated HTTP/2 (it returned 404: nothing routes smelt there yet). Over HTTP/2 the limit becomes 100 streams on one connection, so production won't have the problem once smelt is deployed behind it. **Dev** is plain `http://localhost:8180`, and does.

Two changes:
1. **HTTP/2 in dev:** an HTTPS reverse proxy in the dev compose stack, so the browser talks HTTP/2 to it, and it talks to smelt as before.
2. **One connection per tab:** replies travel on the conversation event stream instead of their own stream. That's worth having even over HTTP/2, because every tab watching a conversation then sees a reply stream in live, not only the tab that sent it. Reloading mid-reply also shows the text so far instead of nothing.

## How

### Part 1: HTTPS proxy for dev

- **A `caddy` service in `docker-compose.yml`** (official `caddy` image), publishing e.g. `8443`, with a small `Caddyfile`:
  ```
  https://localhost:8443 {
      tls internal
      reverse_proxy smelt:8080
  }
  ```
  Checked against Caddy's docs (2026-09-26):
  - it serves HTTP/1.1, 2 and 3 by default;
  - `reverse_proxy` passes WebSockets through (the dev server's hot-reload socket);
  - it flushes `text/event-stream` responses immediately;
  - `tls internal` issues certificates from Caddy's local CA.
- **Trust:** Caddy's docs say installing that CA's root into the system trust store "may fail… when running in a Docker container". So `docs/setup.md` gets a one-time step: copy the root certificate out of the Caddy container's data volume and trust it in the browser, or accept the warning once.
- **Unchanged:** `http://localhost:8180` stays as it is (the browser tier and scripts use it). `SMELT_BASE_URL` (MCP OAuth redirects) is documented for both.
- **Verified by you,** since the dev container's `docker` talks to a sidecar, not your compose stack. I'll give a short check: open `https://localhost:8443`, confirm the protocol is `h2` in DevTools' Network tab, and open more than 6 tabs.

### Part 2: replies on the conversation stream

**Server**
- **`send_message(id, content) -> ServerFnResult<Message>`** becomes an ordinary request:
  - it saves the user's message and returns it, so the tab swaps its optimistic copy for the real one;
  - it starts the turn in a background task, the way a finished command's `wake_conversation` already does, and returns without waiting for the turn;
  - a missing conversation or a send error is still an ordinary error response.
  - `ChatEvent` and the per-send `ServerEvents` go away.
- **New `ConversationEvent`s:**
  - `ReplyDelta { text }`, published as the model streams. `run_turn_bounded`'s `on_delta` publishes to the bus, for every turn, including ones a background notice started.
  - `ReplyReset {}`, when a model call starts, so a new streaming bubble starts per model call. That fixes bug-bash #14, where a multi-step reply streamed as one merged bubble.
  - `TurnError { message }` replaces `ChatEvent::Error`, for any turn. `NotificationDeliveryFailed` stays for background-started turns, since the page words it differently.
  - Finished messages already arrive as `MessagesAppended`, and start/end as `TurnState`.
- **The reply so far** is kept per conversation in memory while a turn streams. A new `get_reply_in_progress(id)` joins the reconnect pull, so a tab that reconnects mid-reply shows the text so far.
- **Channel capacity** rises from 64 events, since deltas are many and small. A tab that still falls behind misses some deltas and catches up at the next `MessagesAppended`; that's already how lag is handled.

**Frontend**
- The streaming bubble and "is a reply in flight" come from conversation events, not the send's own stream: `ReplyDelta` appends, `ReplyReset` starts afresh, `MessagesAppended`/`TurnState { running: false }` clear.
- `send()` becomes: add the optimistic message, call `send_message`, replace the optimistic message with the returned one (or show the error).
- The per-conversation maps from #23 (`replies_in_flight`, `stream_errors`) stay, keyed by conversation, fed from events.
- Stop is unchanged. `stop_turn` ends the turn, and the tab hears `TurnState` and `TurnError { "stopped by the user" }`, which shows "Stopped."

**Result:** a chat tab holds one connection, plus the browsing panel's while that's open.

## Tests (test-first)

**Server**
- `send_message` returns the saved user message without waiting for the turn. The turn runs anyway: its deltas and `MessagesAppended` arrive on the conversation stream.
- Deltas are published in order, with `ReplyReset` at the start of each model call, including a multi-step turn with a tool call between two text replies.
- `TurnError` is published for a failed turn and for a stopped one.
- `get_reply_in_progress` has the partial text mid-turn and is empty afterwards.
- Existing `send_message` tests move to the new shape.

**Browser tier**
- Scenario 8 (switching conversations mid-reply) and 13 (Stop) pass unchanged, with any wait-for-reply steps adjusted.
- **New:**
  - a second tab watching the conversation sees the reply stream in live, before it finishes;
  - a tab reloaded mid-reply shows the partial text;
  - with 7 tabs open on the same conversation, each finishes loading and Stop works. That fails today, and with one connection per tab it fits, up to 6 tabs without the browsing panel. The harness's tab-closing workaround comes out where it's no longer needed.

**Dev proxy:** your manual check, above.

## Which files

- `docker-compose.yml`: the `caddy` service, and a volume for its data (the local CA).
- `Caddyfile` (new, repo root or `docker/caddy/`).
- `docs/setup.md`: dev over HTTPS, trusting the CA, `SMELT_BASE_URL`.
- `src/api/chat.rs`: `send_message`'s new shape, publishing deltas, resets and errors, the in-progress reply, and `get_reply_in_progress`.
- `src/events.rs`: `ReplyDelta`, `ReplyReset`, `TurnError`, and channel capacity.
- `src/frontend/pages/chat.rs`: `send()`, and the streaming bubble fed from events.
- `src/browser_tests.rs`: the new and adjusted scenarios.
- Docs: `api.md` (streaming section rewritten), `frontend.md`, `architecture.md` (the "two streams" description), `testing.md` (the connection-limit notes). Close-out as usual.

## Decisions (2026-09-26)

1. **Stacked on #28** with GitHub's stacked pull requests: stack #30, #28 then #29 (this branch, a draft). Linked with `gh stack link` by PR number. `gh stack sync`/`rebase` aren't used: they rebase and force-push, and this repo merges instead. When #28 merges, `main` is merged into this branch.
2. **Dev proxy:** `https://localhost:8443`, `tls internal`, with a documented step to trust Caddy's root certificate. (Proposal taken; the user didn't choose otherwise.)
3. **Bug-bash #14** (a multi-step reply as one bubble) is included, via `ReplyReset`.
4. **The browsing panel's frame stream** stays its own connection.
5. **The browser tier** stays on HTTP/1.1.
