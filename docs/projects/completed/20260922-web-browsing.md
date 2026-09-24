# Interactive web browsing

**Branch:** `web-browsing` · **Idea:** `projects/ideas/web-browsing.md` (removed) · **Plan:** `projects/plans/web-browsing.md` (removed)

## What shipped

The two pieces the plan scoped, plus infrastructure the review rounds showed was needed, which also changes `webfetch`: an egress proxy for all browser traffic (`src/egress_proxy.rs`), a Chrome launcher that can't leave orphaned processes (`src/headless_chrome.rs`), and a log filter for a chromiumoxide/Chrome protocol mismatch.

### Persistent browsing-session tools

`open_browser_session`/`close_browser_session`/`browser_navigate`/`browser_click`/`browser_fill`/`browser_back`/`browser_read` — a real page that stays open across many tool calls, unlike `webfetch`'s fresh-page-per-call shape. At most one session per conversation, mirroring the sandbox pod's own precedent exactly (implicit resolution, refuses if one's already open). Each action returns the page's current state: rendered text (same truncation shape `webfetch`/`http_request` already use) plus a freshly-tagged list of interactive elements (`data-smelt-el="N"`) the model can reference by index on the next `browser_click`/`browser_fill` call. In-memory only, not persisted — same accepted gap `run_async`'s task registry already has.

### The live panel

A real, two-way interactive view — not a periodic snapshot. `Page.startScreencast` streams real frames over a dedicated per-conversation channel (deliberately separate from `ConversationEvent` — frames are frequent, ephemeral, UI-only data that shouldn't crowd a shared bounded broadcast channel), reference-counted so the screencast only runs while someone's actually watching. The user's own mouse/keyboard on the panel forward to the same real page via `Input.dispatchMouseEvent`/`dispatchKeyEvent`/`insertText` — no locking between the user and the model's own tool-driven actions; both act on one real, shared page.

- An address bar above the frame shows the page's URL and follows every navigation, whoever made it: the model, a click in the panel, a redirect, or an in-page `pushState`. Typing an address there and pressing Enter navigates the session through the same `navigate` (and SSRF guard) the model uses; a bare host gets `https://`. Added after the review rounds, at the user's request.
- Confirmed against the vendored `chromiumoxide_cdp` source before committing to the design: `Page.startScreencast`/`screencastFrameAck`/`EventScreencastFrame` and `Input.dispatchMouseEvent`/`dispatchKeyEvent` all exist with the needed shapes.
- The session's viewport is pinned (`Emulation.setDeviceMetricsOverride`) to exactly the screencast's own bounds, so the frontend's click-coordinate math needs no scale-factor lookup — an on-screen pixel offset within the (fixed-size, non-responsive) frame `<img>` already equals a real frame pixel.
- Runs on `webfetch`'s shared browser and `fetch_guard`'s SSRF guard. The guard applies twice: per page through CDP's Fetch-domain interception, and for all of the browser's traffic through `src/egress_proxy.rs`. The proxy covers popups, WebSockets and service workers, which per-page interception never sees.

**Verification:** 334 `cargo test --features server` tests passing, both build targets clean with no warnings. The browser tier (`cargo test --features "server browser-test" -- --ignored`) passes. It runs 25 browsing scenarios inline in `webfetch`'s real-browser test (they share its process-global browser). Those scenarios cover the session lifecycle, clicks that do and don't navigate (including slow pages and delayed updates), `fill` with any text, screencast frames and viewer lifecycle, input dispatch, URL tracking, and a regression test for every review finding. Alongside them run `webfetch`'s own escape checks, the sandbox-panel harness, and a test that Chrome dies with the process that launched it. Every round was also checked in the real app with a real model: the panel streaming, input, the address bar, HTTPS through the proxy, and refused internal addresses.

## Retrospective

The branch was called done four times. In between, three code reviews found 21 real bugs, a security hole among them, and checking the fixes turned up a leak of orphaned Chrome processes. Nearly everything worth learning is in why they got past the original implementation.

**What worked:**
- **Reproducing each review finding with a failing test before fixing it.** The review rounds' claims were reasoned from reading code, not run. Writing a real-browser test first and running it on the unfixed code turned each claim into a fact. It confirmed most findings, found one worse than reported (the SSRF bypass reached four ways, not one), and disproved another (`file://` was already blocked). It also caught a flawed test before it could vouch for a fix. The same habit found the orphaned-Chrome leak.
- **Checking in the real app, not just the test tier.** Real pages, a real model and real HTTPS found things no fixture did: the address bar showing `chrome-error://…` after a refused load, and confirming the proxy works for real sites.
- **Asking which panel was wanted before building it.** Full remote control rather than a screenshot view was settled before any panel code existed, so it was built once.
- **Fixing at the layer where the problem actually lives.** The SSRF gap was fixed at the network (one proxy for all browser traffic) rather than by chasing popups, WebSockets and workers one at a time. The leak was fixed by the kernel killing Chrome with its parent rather than by shutdown hooks that SIGKILL skips.

**What caused friction, surprise, or rework:**
- **The original tests only covered each piece working on its own.** Most of the 21 review bugs sat at an edge none of them tried:
  - a viewer disconnecting, or two things racing (opens, closes, screencast start/stop);
  - a second session after a first, or a slow page;
  - non-ASCII text, a password field;
  - anything happening outside the one page the guard watched.

  The rest (Enter, hover, modifier keys, wheel units) were input details the tests missed because they checked that CDP commands were sent, not what the page actually saw.
- **Reusing proven infrastructure was treated as adding no new risk.** Per-page interception had already been shown to cover a page's whole lifetime, and its limits were never asked about. A long-lived session the model and user can click around in made those limits far easier to hit. The same gap had been in the merged `webfetch` all along.
- **Three checks passed for the wrong reason:**
  - The log fix was "verified" by an empty log that was empty because *all* logging was off.
  - The click tests used pages that react instantly, so a click that never waited looked fine.
  - The still-page frame test was kept busy by a blinking cursor.

  Each was caught by watching the check fail first, or by asking why it passed.
- **Library behavior was assumed from names and docs instead of read from source**, again and again:
  - `wait_for_navigation` returns at once if the page is already loaded.
  - `type_str` only knows a US keyboard and needs focus first.
  - `EnvFilter` drops its default level once any directive is given.
  - `ServerEvents::new` runs a detached task that never notices a disconnect.
  - chromiumoxide kills Chrome only on drop, and turns popup blocking off by default.

  All of it was in the vendored source the whole time. `webfetch`'s own retrospective named this lesson once already; it didn't stick.
- **The branch grew far past its plan.** Beyond the two planned pieces, it now carries:
  - a log-noise fix;
  - an SSRF proxy and a Chrome launcher, both of which change already-merged `webfetch`;
  - an address bar.

  That's 28 files and about 4,300 lines, in one PR, with one close-out. [development-process.md](../../development-process.md) already warns about exactly this (the `sandbox-visibility` example); it happened anyway, one reasonable-looking step at a time.
- **My own slips cost time:**
  - calling warnings "pre-existing" when this branch caused them;
  - an edit that silently deleted a test helper;
  - a `kill` pattern that matched its own shell;
  - misreading a dev-server rebuild as hot-patching.
- **Still unexplained:** one WASM panic in the panel ("misaligned pointer dereference" in `futures-channel`) on a dev server that had rebuilt mid-session. Three clean runs didn't reproduce it.

**What to change** (proposals — per [the confirm-before-change rule](../../development-process.md#evolving-this-process), none applied yet):
- **Add an edge-case pass to the definition of done** in `development-process.md`. Before calling a feature done, test what happens at each of:
  - the far side disconnecting or closing;
  - two calls racing;
  - a second instance, or reopening after a close;
  - a slow or failing dependency;
  - unusual input (non-ASCII, empty, secrets).

  And for any security boundary, write down what it does *not* cover.
- **Require every check to be seen failing.** Extend the TDD rule from "failing test first" to any test or manual check used as evidence: confirm it fails when the thing it detects is present, before trusting it when it passes.
- **Read the implementation of any library call relied on for timing, lifecycle or defaults**, not just its signature. Make it a named checklist item rather than a retrospective lesson, since it recurred across two projects.
- **Fix bugs found in already-merged code in their own PR.** The proxy and the Chrome launcher should have gone out separately from this feature: they fix `webfetch` on their own merits, and bundling them made this PR harder to review.
- **Name the ungated-wire-types / `#[cfg(feature = "server")] mod server` split as the default shape for new server-only modules** (carried over from the first retrospective): `anthropic::tools`, `events.rs` and `browsing.rs` all ended up there.

**Known gaps left open:**
- The panel's RSX wiring (input handlers, the frame `<img>`, the address bar) is only checked manually; its logic is unit-tested.
- Browsing sessions are in-memory, so a server restart drops them.
- All conversations share one browser profile, and so each other's cookies and site storage. Each launch now gets a fresh profile, but conversations within one server process still share it.
- The unexplained WASM panic above.
