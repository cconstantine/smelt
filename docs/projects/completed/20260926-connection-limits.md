# Fewer connections per tab, and HTTP/2 in dev

**Branch:** `connection-limits`, stacked on `pod-management` (GitHub stacked PRs: stack #30, #28 then #29) · **Plan:** `projects/plans/connection-limits.md` (removed)

## What shipped

Over plain HTTP/1.1 a browser allows 6 connections per host, shared by every tab. A smelt chat tab held one always-open event stream, plus a second for the whole of every reply. With a few tabs open, a new tab couldn't finish loading, or a click (Stop) waited for a reply to end.

### Replies on the conversation stream

- **Sending is an ordinary request.** `send_message` (`start_turn`) checks the conversation exists, starts the turn in the background, and returns. The per-send `ServerEvents<ChatEvent>` stream and `ChatEvent` are gone.
- **Everything a turn does arrives on the conversation's event stream**, in every tab watching it, whoever started the turn:
  - `MessagesAppended` per message, as it's saved (`record_saved`): the user's own first, then each tool call, tool result and reply;
  - `ReplyReset` when each model call starts, then `ReplyDelta`s as its text streams;
  - `TurnState` at start and end, and `TurnError` when a user's turn fails or is stopped.
- **The reply so far** is kept in memory (`REPLIES_IN_PROGRESS`), so a tab that (re)connects mid-reply shows it (`get_reply_in_progress`).
- **The channel holds 1024 events** (up from 64), so a burst of deltas doesn't make a tab miss text. A test showed a subscriber at 64 losing 436 of 500.
- **Result:** a chat tab holds one connection, plus the browsing panel's while that's open. About five smelt tabs fit on HTTP/1.1, leaving a connection for ordinary requests; HTTP/2 lifts the limit.

Along the way, three existing problems are fixed:
- **The sender saw its own message twice.** The turn published one batch at the end, including the user's message, which never replaced the tab's optimistic copy. Likely true since the bug bash made `MessagesAppended` actually deliver. `accept_saved_messages` now swaps the copy for the saved message.
- **Bug-bash #14:** a multi-step reply streamed as one merged bubble, and its tool calls only appeared when the whole turn finished. Each model call now gets its own bubble, and each message appears as it's saved.
- **Stopping a turn a finished command started** was reported as "a background notification failed to reach the model". It isn't any more.

### HTTPS (HTTP/2) for dev

- **A `caddy` service** in `docker-compose.yml` serves the dev server at `https://localhost:8443` (`docker/caddy/Caddyfile`, `tls internal`), so the browser speaks HTTP/2 and the 6-connection limit doesn't apply.
- **Setup:** `docs/setup.md` has the one-time step to trust Caddy's root certificate, and how to check the protocol is `h2`.
- **Production** (homelab's TLS front end for `*.constantlee.us`) already negotiates HTTP/2: a probe of `https://smelt.constantlee.us` on 2026-09-26 got HTTP/2 (and a 404, since nothing routes smelt there yet).
- **Not verified by me.** The dev container's `docker` talks to a sidecar, not your compose stack, and it has no compose plugin. I checked the compose file parses, and followed Caddy's docs for the image, the Caddyfile location, `tls internal` and the proxy's defaults.

### Tests

Each seen failing first:
- **Server:**
  - each message published as it's saved, in order;
  - reply streaming as resets and deltas, one bubble per model call (with a tool call between two replies);
  - the reply so far mid-turn, and cleared after a stop;
  - the burst of 500 deltas;
  - `start_turn` returning before the model answers, while the user's message is published and the turn keeps running;
  - `start_turn` refusing a missing conversation;
  - `TurnError` for a failed and a stopped turn;
  - a stopped woken turn not reported as a failed notification.
- **Frontend:** `accept_saved_messages` replaces exactly one optimistic copy per saved message.
- **Browser tier, scenario 14:**
  - a watching tab sees the reply stream in before it finishes;
  - a tab reloaded mid-reply shows the partial text;
  - the sender sees its message once in the conversation;
  - with five tabs open, a streaming reply and a Stop from another tab both fit.

  It was run against the old send path in a separate worktree, and failed there: the watching tab only saw the reply once it was complete. Scenarios 8 and 13 pass unchanged.

### Not done

- **Your check of the HTTPS proxy:** `docker compose up`, trust the root certificate, open `https://localhost:8443`, and confirm `h2` in DevTools' Network tab.
- **Deploying smelt to `smelt.constantlee.us`** isn't part of this.
- **The browsing panel's frame stream** is still its own connection while the panel is open (decided in the plan: HTTP/2 covers it in production).
- **The browser tier stays on HTTP/1.1,** so the limit stays visible there.
- **No real-model check of the new streaming.** The browser tier's slow fake model covers streaming, reloads and Stop, but no live model ran.

## Retrospective

**What worked:**
- **Checking the claims the design rested on before relying on them** (the new plan-phase step): MDN for the SSE connection limit and HTTP/2's 100 streams; a probe showing the homelab endpoint already speaks HTTP/2, which moved option 1 from "production fix" to "dev only"; Caddy's docs for its defaults and the Docker image's layout. A WebSocket alternative was dropped because the sources didn't show it would help.
- **GitHub stacked PRs, used without the parts that rebase.** `gh stack link` by PR number joined #28 and #29 into a stack without pushing or rebasing; `sync`/`rebase` would force-push.
- **Running the new browser scenario against the old code in a separate git worktree.** It proved the scenario detects the old behaviour without breaking the working tree, and exposed the broken intermediate commit below.
- **Fixing the event model, not just the connection count.** The same change fixed three older problems: the doubled message, bug-bash #14, and the mis-reported stop.

**What caused friction, surprise, or rework:**
- **The plan's arithmetic was wrong.** It promised seven tabs would fit with one connection per tab; with a limit of 6, they can't. Worse, a tab's own loading needs a connection besides its stream, so the real ceiling is about five. The browser tier caught it (a tab that never loaded), and an instrumented run found the rest: an earlier scenario's tab never closed, still holding a stream.
- **One commit on this branch doesn't build for the web** (`224aff9`: new events without the page handling them). I ran only the server tests between commits, and the next commit fixed it. PRs here are squash-merged, so it won't reach `main`, but it broke the first worktree run.
- **A count over the whole page misfired:** the sender's message also appears as the conversation's sidebar title. Page-wide text checks need scoping to the element that matters.

**Process change (confirmed and applied to development-process.md, under Rules):**
- Build both targets before each commit, not only before calling a feature done: `cargo check --no-default-features --features web --target wasm32-unknown-unknown` alongside the server tests. A commit that doesn't build breaks bisecting and "run the test against the old code" checks like the one above.

**Bug bash: still due** (four projects since the last one).
