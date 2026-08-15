# Sandbox visibility: watch pods/terminals/commands live — plus a fair amount more

**Branch:** `sandbox-visibility` · **Idea:** `projects/ideas/coding-session.md` (kept — file read/write tools, a coding-oriented system prompt, and `git clone`/credential wiring are still open) · **Plan:** `projects/plans/sandbox-visibility.md` (removed)

## What shipped

The plan's own scope — a live panel showing every pod/terminal the model
has and each terminal's command history, streaming as it happens — plus
several rounds of follow-on requests that landed on the same branch as
direct asks mid-session rather than through their own idea/plan docs. All
of it is listed here since it shipped together; see the retrospective for
whether that was the right call.

**The sandbox panel itself** (the plan's original scope):
- One-shot snapshot (`get_sandbox_state`) plus live events
  (`SandboxPodUpdate`/`SandboxTerminalUpdate`/`SandboxCommandUpdate`),
  same pattern the background-tasks panel already used. A terminal
  hydrates its `HISTORY_LIMIT` most recent commands (not just the latest),
  each with its own bounded stdout/stderr tail.
- **stdout and stderr are fetched and capped independently, then merged
  back into one true chronological sequence for display** — the first
  version showed "all stdout, then all stderr" regardless of when either
  was actually written (`TerminalLine` gained a `seq` field specifically
  so two separately-fetched tail windows could be re-sorted into the
  order they happened in). Caught from a real report against a real
  terminal, not a test.
- **More than one pod gets a tab bar**, not a stack — the terminals
  themselves already used a flex chain that hands a bounded height down
  to each terminal's own scrollbar (single scrollbar per terminal, no
  panel-level or page-level one), which the tabs preserve by only ever
  rendering one pod's terminals at a time.
- **Responsive layout**: side-by-side (chat left, sandbox right) at or
  above 1024px, stacked (sandbox on top, chat on the bottom) below it —
  DOM order alone drives the stacked case, `order: -1` on `.chat-main`
  flips it for the row case, no JS involved.
- `reconnect_if_needed`'s "pod not found" branch now runs the same crash
  cleanup a failed `connect()` already did (previously it just errored,
  leaving a still-live terminal row wedging `terminate_pod` behind
  `TerminalStillExists` forever with no pod left to reconnect to — hit for
  real, fixed, regression-tested against the real cluster). A new
  `try_reconnect`, called eagerly by `get_sandbox_state`, also means a
  pod that survived a smelt restart with a perfectly healthy agent
  doesn't sit showing "disconnected" until the model happens to touch it
  next.

**Follow-on work that landed on the same branch:**
- **Conversation URL routing** (`/conversation/{id}`, `/` for nothing
  selected) — a refresh, bookmark, or direct link now lands back on the
  same conversation. Surfaced a genuine Dioxus reactivity bug along the
  way: a `use_effect` syncing a route param into a plain `Signal` looked
  reasonable but silently broke, since effects only re-run on a *tracked*
  read and a plain prop isn't one — switching from one conversation
  straight to another (same route variant, different `id`) left every
  downstream `use_resource` frozen on whichever conversation loaded
  first. Fixed by deriving the selection with `use_memo` over
  `router.current::<Route>()` instead, a genuine tracked read. See
  `frontend.md`.
- **Extended thinking** (`ContentBlock::Thinking`, collapsed-by-default
  disclosure). On by default, but a local Ollama-served `gpt-oss` model's
  Anthropic-compatibility shim can fail to keep its reasoning separate
  from a tool call's own JSON — hit twice, live, with two different
  failure shapes (reasoning-prose-then-JSON, and the model just writing
  invalid JSON on its own). `run_turn` now retries: drop `thinking` once,
  then a bounded number of plain regenerations, before giving up. Live
  verification against the real Ollama server (22 real attempts across
  two rounds) never reproduced either failure again, though that's a
  small sample against a reportedly intermittent bug, not proof it can't
  recur.
- `MAX_TURNS` raised 5 → 10,000 and `RESPONSE_TIMEOUT` raised 90s → 10
  minutes — both hit for real during a genuine multi-step sandbox-terminal
  session and a cold-loading local model, not anticipated in advance.
- An auto-scroll-to-bottom gap: the transcript's stick-to-bottom effect
  only tracked `messages()`/`streaming_text()`, so the sandbox/tasks
  panels appearing *after* messages had already loaded (a separate,
  later fetch) shrank the transcript's allotted height without
  re-triggering the scroll — left a small persistent gap at the bottom.
  Fixed by also tracking `tasks()`/`sandbox_pods()`/`sandbox_terminals()`.

**Verification:** 138 `cargo test --features server` tests, WASM check
clean, and `src/browser_tests.rs`'s one automated end-to-end browser test
(real Postgres, real k3s, real headless Chrome) — which a final review
caught failing on an assumption the tabs feature had broken (it expected
every pod's terminals to render simultaneously); fixed to click through
the tab bar instead, and it now genuinely exercises tabs, live streaming,
targeted terminal removal, and reload-reconstruction together.

Explicitly not done (per the idea file's still-open items): file
read/write tools, file-edit diffs in the transcript, a coding-oriented
system prompt, `git clone`/credential wiring.

## Retrospective

**What worked:**
- **Testing against real infrastructure (real k8s, real Ollama) caught
  bugs a mock never would have** — the `reconnect_if_needed` crash-cleanup
  gap, both Ollama tool-call corruption shapes, and the browser-test
  tabs regression were all found this way, not by unit tests.
- **A deliberate final review before calling the branch done caught a
  real regression** — the automated browser test had been silently
  broken since tabs landed, and nothing had re-run it in between. Worth
  being a standing step, not a one-off.
- **The "temporary `#[ignore]`d diagnostic test against the real
  environment, clean up, remove" pattern**, reused repeatedly (a stuck
  pod, a stale-disconnected terminal, live Ollama reproduction attempts),
  was cheap and effective for live debugging without leaving throwaway
  code behind.

**What caused friction, surprise, or rework:**
- **Nothing on this branch was committed until the final review caught
  it.** `git branch -a` showed `sandbox-visibility` pointing at the exact
  same commit as `main` — an entire multi-session feature (everything
  above) sitting only as uncommitted working-tree state, discoverable
  only by explicitly diffing against `main`. Two files
  (`src/browser_tests.rs`, this plan doc) were referenced by other
  committed docs and code as if they already existed in history.
- **Managing the user's own `dx serve` process caused real, avoidable
  friction.** Killing it to pick up server-side changes once left
  orphaned child processes that blocked the user from starting their own
  — after which the working pattern became "build the code, tell the
  user, let them restart," which cost trust and a full round-trip to
  arrive at rather than being the default from the start.
- **Scope grew substantially past the plan without a new idea/plan doc
  for any of it** — routing, thinking, the Ollama retry ladder all arrived
  as direct mid-session requests. Convenient in the moment, but it means
  this single completed doc now covers several genuinely separable
  pieces of work, and the plan's own "What"/"How" no longer describes
  most of what the branch actually contains.
- **Characterizing the Ollama thinking bug took several iterations to
  get right**: an initial live-verification attempt asked for 10 total
  real model calls before realizing a single cold-load call alone could
  take minutes; a follow-up attempt hardcoded a real API key into a test
  file (correctly blocked by the permission classifier, fixed to load
  `.env` at runtime instead); a later attempt was killed as "stuck" at
  ~40 minutes of silence when it was actually still legitimately
  in-flight, costing a wasted run.

**What to change (proposals — confirmed and applied to
`development-process.md`):**
- Commit at natural milestones during a long session, not only at the
  very end — the existing "one commit per finished feature" convention
  doesn't cover a session where several individually-shippable pieces
  land back-to-back on one branch over many hours.
- When a request substantially exceeds the current plan's scope, consider
  flagging it as worth its own idea/plan doc before folding it in — keeps
  each close-out's "what shipped" and retrospective scoped to what they're
  actually about, rather than one doc covering unrelated work.
- Treat "does the automated browser tier still pass" as part of the
  definition of done for any change touching the DOM structure it
  asserts against, not just a final-review afterthought.
- Never write a real secret into a source file, even temporarily for a
  throwaway diagnostic — load it from the environment/`.env` at runtime
  instead. Caught by tooling this time; shouldn't rely on that.
- Before killing/restarting a long-running process the user started
  themselves (e.g. `dx serve`), default to asking rather than acting —
  the cost of a wasted round-trip asking is much lower than the cost of
  leaving the user's own session in a broken state.
