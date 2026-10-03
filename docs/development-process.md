# Development Process

These rules are mandatory. Follow them in order. Do not skip steps.

---

## Where work is tracked

Ideas, plans and finished projects live in Linear, not in the repo: team **Smelt Agent** (ticket ids `SME-N`), project **smelt**. Use the Linear MCP tools to read and update them.

| Status | Meaning |
|---|---|
| **Backlog** | An idea: what the user wants, not yet planned. |
| **Todo** | Planned: the ticket has an approved plan and is ready to be worked on. |
| **In Progress** | Being implemented on a branch. |
| **Done** | Merged and closed out: the ticket records what shipped and the retrospective. |
| **Canceled** / **Duplicate** | Dropped, or folded into another ticket. |

Each ticket gets one label: **Feature**, **Improvement** or **Bug**. Related tickets are linked as related rather than only mentioned.

The project's **Current state** document (on the smelt project in Linear) describes current features, architecture and goals. Its **Feature checklist** document lists every shipped feature with the steps to check it through the web UI, one row per ticket.

Linear's markdown differs from GitHub's in ways that bite: don't hard-wrap lines (a wrapped line starting with `+` or `-` becomes a list item), put file names in backticks (Linear turns a bare `name.rs` into a web link), and link repo files by their GitHub URL, not a relative path.

---

## Phase 1: Plan

**Do this before writing any code, any tests, or any files other than the plan itself.**

1. Read the user's request carefully
2. Read the ticket, and the project's **Current state** document, to understand the request and the current codebase
3. Ask clarifying questions if the request is ambiguous — do not assume
4. **If the work depends on an outside service or tool, check it before presenting options.** For each claim a choice rests on (whether it's still available, pricing and free tiers, what signup or access it needs, the API's request and response shape), check the provider's current docs and cite them. Don't answer from memory with a general "worth checking" caveat. Also look at how a comparable tool solves the same problem, in its source rather than its marketing. On `websearch`, provider answers from memory had three wrong or stale claims, and a three-provider plan was written on top of them. Reading opencode's source then showed Exa's keyless MCP endpoint, which cut the project to about 60 lines.
5. Create a branch in a worktree of its own: `git branch <short-slug> main`, then `git worktree add ../smelt-<ticket> <short-slug>`, and work there. Never `git checkout` in the main checkout: the user's dev server may be serving it. On SME-43 this step said `git checkout -b`, and following it switched the user's checkout. A new worktree fails `scripts/check.sh` until it has a sandbox image record; `scripts/cluster-doctor` takes the main checkout's when the agent sources match.
6. Write the plan into the ticket's description, under a `## Plan` heading after the idea's own text (create a ticket first if the work has none). The plan contains:
   - **Branch:** the branch name created in step 5
   - **What** is being built and why
   - **Which files** will be created or modified (be specific)
   - **How** it will be implemented: data model, API shape, UI flow
   - **State model**, for a feature that keeps state (database rows with a status, files in the sandbox, decisions the user makes): each record's states, what moves it between them (the user, the model, a Stop, a restart, a retry, a timeout, another conversation), and who else sees each change (other conversations, other tabs, the pod). For state in the sandbox, say what survives a pod restart and what's lost. For each transition, also say what happens if the operation that makes it fails partway: a delete that errors after the mark is set, a send that fails after the event was taken. For a record that several callers write, also list every writer: each path that creates, retries or finishes it (a retry and a fresh request for the same thing, a failure path as well as success). The races sit between writers. List every caller that reads shared state too (a remembered setting, a cached decision), and what each does with a value another caller just changed: on SME-93 all three review rounds found their bug at a reader of the remembered per-provider thinking choice, not at a writer. For each check a caller makes before acting on a record (is it waiting? is it answered?), also say what can change between the check and the act, and who can change it: on SME-34 a turn checked a pending question when it started and again only when calling the model, and an answer recorded in between was neither waiting nor taken (round 1's high finding). The tests then cover each transition, not just the happy path. On SME-32 none of this was written down: a re-clone step was built and then thrown away when the user asked why `/workspace` didn't outlive the pod, and ten code-review rounds found about 45 bugs, most of them transitions nobody had listed (retry, interrupt, trust given from another conversation), several made by the previous round's local fixes. On SME-53 the plan listed a torn-down mark's transitions but not a teardown that fails after setting it, and a failed pod delete left a live pod refusing every connect. On SME-86 the plan had a repo row's states but not its writers, and both review rounds found another writer racing the retry it fixed: a fresh clone into the same directory, and a failed attempt's error path.
   - **Where new UI goes,** for anything new on screen (a banner, a panel, a button): where it sits at a laptop width and at a phone width, and what it may cover. On SME-43 the plan placed a reload banner "at the top"; on a phone it covered the Conversations button, found only by a screenshot. A UI fix's plan also names each layout the fix depends on (laptop and phone widths, which side panels are open, where they sit) and what the fix does in each. On SME-75 the fix for text moving under the pointer had to hold with the side panels open and at phone width, which the plan hadn't listed.
   - **What each caller does in each state,** as a small table: for every state (none, starting, ready, broken...), what each caller of it does (waits, fails fast, starts it, retries, gives up). The states and transitions alone don't say this. On SME-35 the plan listed a language server connection's states, and "an edit never starts a server", but not that an edit must never *wait* for one to connect. Most of its 15 review findings were at those cells: a failed start left running, an edit waiting two minutes, a spent retry budget that a restart didn't reset, a "connecting" message repeated forever.
   - **A row for work already in flight.** For any setting a running operation reads (a turn's model, a pod's limits), say what the operation in flight does when the setting changes under it (keeps what it read, or picks up the change, and when), and which record says what it actually used. On SME-72 the plan had the model's states but not this, and the first review's top finding sat exactly there: the switch point for a model's signed reasoning was recorded when the user picked a new model, while a turn still running kept writing for the old one past it.
   - **Open questions or tradeoffs** you are not sure about. When a tradeoff says "only X sees the difference", name a real X and check it before writing it down. On SME-90 the plan stopped at "only header-less clients see the difference" without asking who they are; review found webhooks to production's https previews refused.
7. Show the plan to the user (link the ticket) and **stop**
8. **Wait for explicit approval** — a response like "looks good", "yes", or "go ahead". The user may also leave feedback as comments on the ticket.
9. Once approved, move the ticket to **Todo**; move it to **In Progress** when implementation starts
10. Do not write any implementation code or tests until you receive that approval

If the user requests changes to the plan, update the ticket's plan and show it again. Repeat until approved.

---

## Phase 2: Implementation (TDD)

Work through the plan one behavior at a time. For each behavior:

### Step 1 — Write a failing test

Write the test before writing any implementation code.

The test must fail **because the behavior does not exist yet** — not just because the code does not compile. If the code does not compile, that is not yet a failing test; it is an incomplete test. Get it to compile first (with stub implementations returning dummy values), then confirm it fails at runtime.

A good failing test:
- Calls the real function or module being built (not a placeholder)
- Asserts the specific outcome the behavior should produce
- Fails with a clear message that points to the missing behavior
- Would pass once the behavior is correctly implemented and fail if it regresses

A bad failing test:
- Fails only because it calls a function that does not exist yet
- Asserts `true` or uses `assert!(result.is_ok())` without checking what is inside
- Would pass with any non-panicking implementation
- Tests the wrong thing (e.g., tests the test setup rather than the code)

Show the failing test output to the user before moving on.

### Step 2 — Write the minimum code to make it pass

Implement only what is needed to make the test pass. Do not add features, abstractions, or handling for cases the test does not cover yet.

Run the test and confirm it passes.

### Step 3 — Refactor if needed

Clean up the implementation while keeping the test green. Run the test again after refactoring to confirm it still passes.

### Step 4 — Repeat

Move to the next behavior in the plan. Write the next failing test. Do not skip ahead.

---

## Rules

- **Write the failing test first for logic-bearing code** — anything with branching, computation, parsing, or edge cases. Never write that implementation before its test exists.
- **Mechanical mirror code is the one exception.** Code that is a near-verbatim copy of something already covered by an equivalent test — e.g. CRUD for a new table that mirrors an existing table's CRUD — may be written alongside a characterization test instead of strictly test-first. The test must still assert real behavior (e.g. a create/read/update/delete round-trip), not just that it compiles. When you take this path, say so.
- **Never move to the next behavior before the current test passes**
- **Never show the user a passing test without having first shown the failing version**
- **See every check fail before trusting it when it passes — not just new tests.** This covers a regression test for a reported bug, a manual check, and a log grep used as evidence alike: first confirm it fails when the thing it detects is present. For a bug, that means running the new test on the unfixed code. When a check for a suspected bug passes unexpectedly, find out why before believing it. On `web-browsing`, three checks passed for the wrong reason:
  - a log fix "verified" by an empty log that was empty because *all* logging was off;
  - click tests whose pages reacted instantly, so a click that never waited looked fine;
  - a still-page frame test kept busy by a blinking cursor.
- **Verify external crate APIs against the source, not memory.** Read the crate in `~/.cargo/registry/src` (or `cargo doc`) before calling unfamiliar methods, especially for fast-moving or less-documented APIs. Macro-generated or re-exported items don't show up in a `grep` for `fn` and are easy to hallucinate. **When the code depends on a call's timing, lifecycle or defaults, read its implementation, not just its signature and doc comment** — what makes it return, what it does on drop or disconnect, what it turns on or off by default. `web-browsing` hit several of these, all visible in the vendored source:
  - `wait_for_navigation` returns at once if the page is already loaded;
  - `type_str` only knows a US keyboard and needs focus first;
  - `EnvFilter` drops its default level once any directive is given;
  - `ServerEvents::new` runs a detached task that never notices a disconnect;
  - chromiumoxide only kills Chrome on drop, and turns popup blocking off by default.

  `webfetch`'s retrospective had already named the same lesson.
- **A request that carries a credential in a custom header doesn't follow redirects.** HTTP clients strip only the standard auth headers on a redirect to another host: reqwest drops `Authorization` and cookies, but keeps `x-api-key` and any other custom header (checked in reqwest 0.12's `remove_sensitive_headers`). A redirecting endpoint could then hand the credential to a host it wasn't entered for. Build the client with `redirect::Policy::none()` (as `anthropic::stream::Endpoint::client` does, SME-72), or send the credential only as `Authorization`. The same goes for a credential stored for one address: a new address needs it entered again.
- **Verify a wire protocol's own structural constraints the same way — against its real spec, not an assumed shape.** Before writing code that builds or extends a request/message sequence for an external API, check what shape constraints it actually enforces (role-alternation rules, a required first role, ordering requirements, and similar structural rules), not just what data belongs in each field. Discovered on `auto-compaction`: the first design for inserting a compaction summary assumed one synthetic message would do, but Anthropic's Messages API requires `messages` to start with `user` and strictly alternate roles — a single message can't satisfy both "starts with user" and "ends with something to respond to" at once. Caught by reasoning through the real constraint on paper before writing any code, not by a failing test; the fix was a three-message sequence (`user` placeholder → `assistant` summary → `user` continuation) instead of one.
- **Spike the riskiest assumption first.** When a planned phase depends on an unproven external or architectural assumption (a runtime, tool, or framework behavior), validate it with a minimal spike before building dependent infrastructure. **When the assumption is specific to a container or namespace runtime — signal handling, PID 1 behavior, process groups, filesystem/network isolation — the spike must run inside the actual target container, not just on the host.** A host-level spike proving the same mechanism isn't sufficient evidence: container-specific kernel/runtime behavior (e.g. PID 1's kernel-forced `SIG_IGN` on any signal it never installs a handler for, inherited by every descendant process) can silently invalidate a result that held on the host. Discovered the hard way on `sandbox-terminal`: a `send_signal` mechanism verified working in a local, non-containerized spike silently did nothing the first time it ran for real, because of exactly this.
- **For a failure-mode or resource-limit assumption specifically, spike with a workload shaped like real usage first — not an artificially controlled trigger.** A controlled trigger (a synthetic memory bomb, a single isolated failure) is for *confirming* a mechanism once you already know roughly what to expect; it's not a substitute for finding out what the failure actually looks like under real conditions, and can give a misleadingly narrow picture if it's the only spike run. Discovered on `sandbox-oom`: a first spike (a single-process memory bomb under a real memory limit) found a narrow, misleading picture of an OOM kill's blast radius (just the one process dying); a second spike using a real `cargo build` found the actual dominant behavior (the whole pod dying together, via `memory.oom.group=1`) that invalidated three detection designs already built on the first spike's incomplete picture.
- **When a real-environment-only failure resists two consecutive fixes built on the same theory, stop iterating on that theory.** Add direct observability into the real environment (logs, `kubectl describe`/events, whatever the environment actually exposes) before attempting a third variation — "same failure, same timing, different magnitude of the thing I changed" is itself a signal the theory targeted the wrong mechanism, not that the fix just needs tuning further. `ci-tests-in-pr` spent two full iterations (raising a timeout, then lowering a CPU request) on a sandbox-test `Timeout` in CI that a local repro made *look* like CPU contention — both fixes failed identically, at the same elapsed time regardless of the CPU size changed, which in hindsight was the tell. A diagnostics-on-failure CI step (dumping real pod/event state) found the actual cause — insufficient memory, never CPU — on the very next run.
- **Verify Kubernetes subresource RBAC verbs against reality, not the resource name or older examples.** Before granting a Role a verb for a subresource (`pods/exec`, `pods/portforward`, `pods/attach`, ...), confirm the actual HTTP verb the client library sends — via a raw HTTP probe or a `SelfSubjectAccessReview` — rather than inferring it. A WebSocket-based subresource call is often an HTTP `GET` (authorized as `get`), not a `POST`/`create`, even when the operation conceptually "creates" a connection — `sandbox-terminal`'s `pods/portforward` grant used `create` (a reasonable but wrong guess) and 403'd every time until a raw probe against the API server showed the real verb.
- **Bound the boundaries, not every await.** An await only stalls if something it transitively waits on can stall, and that happens where control leaves the process. Put a timeout there — once, at the shared client or wrapper — and let interior awaits inherit it. Don't add timeouts to in-process waits; a hang there is a bug a timeout would hide. At each boundary, also handle every non-success terminal state explicitly (cancelled, superseded, truncated — not just `Err`), and add `tracing` at the seam so a stall identifies itself from a console read.
- **Two `async fn`s that call each other (directly or through a longer cycle) defeat rustc's `Send`-auto-trait inference**, surfacing as a cryptic `cannot satisfy \`impl Future: Send\`` error pointing at an unrelated line, not at the cycle itself. Break it by making at least one side return a boxed, type-erased future (`Pin<Box<dyn Future<Output = T> + Send>>`) instead of relying on `async fn` sugar's opaque return type. `turn::run_turn` and `anthropic::tools::execute` are a concrete instance of this shape (a turn loop that dispatches tools, one of which can itself trigger another turn) — expect the same pattern anywhere a conversation loop hands control to a tool/proxy that can call back into it, which a real (non-throwaway) tool-use feature will.
- **Surface fallback outcomes on user-visible flows.** When a feature degrades gracefully — a parser returns `None`, a lookup misses, an optional enrichment fails — the UI must say what happened. A silent fallback is indistinguishable from the code not running at all, both to the user and to whoever debugs their report. Best-effort is fine; invisible is not.
- **Fixture real artifacts — don't re-synthesize them.** When a bug is reproduced from a real artifact (an API response, a wire capture, a malformed input), check the artifact's actual bytes in as the regression fixture. A hand-built synthetic fixture encodes the same assumptions that produced the bug.
- **A mock of an outside service also covers what the real service sends today,** not just the minimum that exercises the code path being built. When a feature that talks to the service is touched, fetch the service's live responses (metadata, headers, optional fields) and check the mock still has a variant for each shape. SME-16's mock OAuth server deliberately served no discovery metadata, which exercised rmcp's fallback path but meant no test could reach its issuer checks. When GitHub started publishing metadata that required `iss`, Connect broke for GitHub and Linear, and no test noticed (SME-65).
- **Commit at natural milestones, not just at the end of a long session.** A session that ships several individually-shippable pieces back-to-back (a bug fix, a UI change, a new feature) risks leaving all of it as uncommitted working-tree state indefinitely — `sandbox-visibility`'s entire branch sat uncommitted, days of work, until a final review caught it by diffing against `main`. Commit once a coherent, working piece is done rather than treating a whole session as one unit.
- **Stage files by name. Never `git add -A`, `git add .` or `git commit -a`.** The user may be editing the repo at the same time, and a catch-all add sweeps their in-progress work into your commit. On `system-prompt`, a new idea file and an edit the user made mid-session landed in a code commit, and were only split out because the commit happened to be read afterward. Check `git status` before committing, and leave alone anything you didn't change.
- **Every commit builds for both targets.** Before each commit, not only before calling a feature done, run the web check and the server build alongside the server tests. `scripts/check.sh` does all three and fails on any failure (a warning in either build included), so gate the commit on it rather than reading output by eye. Before the tests it runs `scripts/cluster-doctor`, which fails at once if the cluster's node lacks a sandbox image or the imported agent was built from other sources than the working tree's, with what to do:
  ```bash
  scripts/check.sh && git commit ...
  ```
  On SME-41 a commit went in over a failing test because the command ran the tests and then committed regardless; the script is the fix. The same goes for any command whose result you act on: never end it with something that replaces its exit status (`| tail`, `| grep`, `; echo exit=$?`). Save the output to a file and check the command's own status (`scripts/check.sh > log 2>&1; rc=$?`). On SME-32 a failed image build reported success through a trailing `echo`, and `scripts/check.sh | tail` let a commit through with a build warning. On SME-33 a commit passed every test and still didn't build: `ListParams` was imported only under `#[cfg(test)]`, so the test build compiled and the server binary didn't. The script now builds the server binary on its own too.
  A change to a type the page matches on (a new `ConversationEvent` variant, say) can pass every server test and still break the web build. A commit that doesn't build breaks bisecting, and breaks checking a new test against older code in a separate worktree. On `connection-limits`, one commit added events without the page handling them; only server tests ran between commits, so it went unnoticed until a worktree run of the old code failed to compile.
- **A commit that only moves code can be gated on building every target,** with the full `scripts/check.sh` run on the branch's tip, when a script proves the move (each item appears verbatim, once, in its new place) and the script is kept in the PR description or ticket. Every other commit still runs the full check. On SME-56 a full locked check per move commit cost most of a day of shared lock time for commits whose content a script had already shown unchanged.
- **Start a refactor of a file once the open PRs changing it have merged,** or plan to regenerate its move commits by script instead of rebasing them. On SME-56 the split of `sandbox.rs` was stacked on two branches still in review; both merged mid-work and `main` moved twice more, so its first commit was regenerated three times and two queued gates were thrown away.
- **Say in the PR when it adds, removes, renames or rewords one of smelt's own tools.** The tool list is part of what the model's earlier reasoning is bound to: on an Anthropic account that checks preserved thinking, any change to it fails ongoing conversations' next turn until SME-93's fix is in. SME-54 removed twelve tools.
- **A rule written into the docs comes with a sweep of the code it covers.** When a change adds "every X does Y" to a doc, search the code for every X in the same change, fix or list what doesn't, and say what the sweep found in the PR. On SME-81 the docs gained "every user-facing error goes through `server_error_message`"; the sweep found sites the ticket hadn't named.
- **Flag when a request substantially exceeds the current plan's scope**, rather than silently folding it into the branch already in progress. `sandbox-visibility`'s plan covered a live pod/terminal panel; the branch it shipped on ended up also covering conversation URL routing and extended thinking, neither mentioned in that plan — convenient in the moment, but it left one close-out doc and retrospective covering several unrelated pieces of work instead of one each. Consider whether a substantially-different ask deserves its own idea/plan doc before starting it.
- **Fix bugs found in already-merged code in their own PR**, even when a review of the current branch is what found them. On `web-browsing`, the SSRF proxy and the Chrome launcher fixed gaps in already-merged `webfetch` but shipped inside the browsing PR, which grew it to 28 files and one close-out doc covering several unrelated changes. A separate PR keeps each fix reviewable on its own and lets it merge without waiting on the feature.
- **Give a new server-only module shared wire types from the start.** Put the types that cross the client/server boundary at the top level, unconditionally compiled, and the server logic in an inner `#[cfg(feature = "server")] mod server { ... }` with `pub use` re-exports. Decide this when the module is created, not when a server function first needs one of its types on the `web` side. `anthropic::tools`, `events.rs` and `browsing.rs` all ended up in this shape, and `browsing.rs` needed a mid-implementation rewrite to get there.
- **Test every type that crosses the client/server boundary at that boundary.** A server-function payload or `ConversationEvent`-style enum gets a test that serializes one of each variant to JSON and back. A test that inspects the value inside the server proves nothing about whether it can be sent. On the bug bash, `ConversationEvent::MessagesAppended(Vec<Message>)` was a tuple variant in an internally tagged enum, which serde can't serialize. Every send failed silently, so no tab but the sender's ever got a message live, and every existing test passed because none of them serialized the event. `events::wire_tests::test_every_event_round_trips_through_json` is the pattern; add the new variant to it when adding one.
- **When a fresh, unrelated request arrives mid-branch, decide how to package it (its own commit? its own branch?) at the point it arrives, not just at the end of the session.** The packaging question is easy to answer in the moment and gets harder to reconstruct later, once several such requests have piled up in the same working tree. `file-tools`' branch picked up a live bug fix, `ANTHROPIC_AUTH_TOKEN` support, and a sandbox resource-limit fix as separate mid-session asks, all landing in the same uncommitted working tree — sorting them into three clean commits (and, for the bug fix, its own already-merged PR) only happened as a deliberate untangling step at the very end, using `git stash` and a reconstruct-then-recommit dance to split files that had picked up more than one concern's edits.
- **Never write a real secret into a source file, even temporarily for a throwaway diagnostic.** Load it from the environment (`std::env::var`, `dotenvy::dotenv()`) at runtime instead. A live-verification test on `sandbox-visibility` once had a real API key typed directly into it — caught by tooling that time, but the discipline shouldn't depend on that safety net existing.
- **Default to asking before killing or restarting a process the user started themselves** (e.g. a long-running `dx serve`). Killing one to pick up a server-side change once left orphaned child processes that blocked the user from starting their own afterward — asking first costs one round trip; leaving the user's own session broken costs much more. **The same hazard has an implicit form, with no `kill` command involved at all:** investigating a live-app symptom by editing source files in a repo `dx serve` is actively watching triggers a real rebuild-and-restart on every save — even a `#[cfg(test)]`-only edit — which can interrupt a concurrent real user request exactly like an explicit restart would. `mcp-servers`'s flakiness investigation chased a symptom that stopped reproducing right around when the debugging session stopped actively editing files; that's a specific signal worth naming and checking for, not evidence the bug quietly self-healed.
- **Stop only processes you started, by the PID you recorded when you started them.** Never kill by name or pattern (`pkill -f`, `grep … | kill`). A pattern also matches the shell running the kill command whenever the pattern appears on its own command line. That happened twice, once on `websearch` and once on `system-prompt`, even after a retro named the risk. It can also match the user's own processes, since their dev server runs the same binaries. So start a background server in a way that prints its PID, note it, and stop exactly that PID and its children later. **Then check the PID is the server itself** (`readlink /proc/<pid>/exe`). Backgrounding `cd dir && ./server &` records the subshell's PID, and `setsid` forks, so the server gets another; start it with `exec` (`(cd dir && exec ./server) &`). On SME-43 both happened: the kill missed, the old server stayed up, and the new one couldn't bind the port. **Before editing source files for a live check, look for a dev server of the user's watching the repo** (`ps` for `dx serve`), and say so if one is running: every save rebuilds and restarts it, as described above.
- **A fix that adds state lists that state's transitions first,** the same way a plan's state model does (see Phase 1). This covers a kept error, a new notice or a new lock as much as a new table. Write the transitions in the commit or the ticket before the code, including what happens when the step that makes one fails partway, and test each one. On SME-51 both code-review rounds found edges of state the bug fixes had added: a kept turn error cleared at the wrong moment, and a stop notice saved twice.
- **Run hands-on checks against a server that doesn't watch the files you're editing.** `scripts/check-server start [REF] [-- KEY=VALUE ...]` builds and serves the branch from a separate git worktree (`../smelt-check`, or `$TMPDIR/smelt-check` when that isn't writable; port 8081), waiting for a real answer rather than `dx`'s 500 while it builds, and `scripts/check-server stop` stops it and everything it started. To move it to a newer commit, `stop` it and `start` again. If the dev database has migrations from another branch, smelt won't start against it (every page is a 500); `CHECK_SCRATCH_DB=1` serves from an empty database of the check server's own, dropped on `stop`. On SME-65 a branch off `main` hit this after another session's branch had migrated the dev database. The script also points the server at headless Chrome (`BROWSER_CHECK_CACHE`), which `webfetch` and browsing sessions need; on SME-51 a hand-made worktree lacked it. It also stops `dx serve`'s server child along with `dx` itself: stopping `dx` alone once left the server running. A server started in the working tree rebuilds and restarts on every save: on SME-41 that killed a nine-minute model turn, dropped an in-memory browsing session, and left pages on a stale stylesheet until a manual restart. The same hazard was already noted on SME-17 and SME-25.
- **When a change makes a previously-inert code path start firing calls into a different subsystem, check whether tests exercising that path now need the same test isolation that subsystem's own tests already rely on** — especially when the call is indirect (a detached background task, not something the test itself invokes), since it's easy to miss precisely because the affected test's own code never mentions the subsystem at all. `terminal-exit-notify`'s real-cluster integration test hung the first time it ran: giving every terminal-command completion an active push into `turn::wake_conversation` meant that test's already-existing commands now made genuine (and, in this sandboxed environment, un-routable) calls to the live Anthropic API — it needed the same `ANTHROPIC_BASE_URL` redirect `turn`'s own mock-upstream tests already use, even though `sandbox.rs`'s test had never had to think about Anthropic before.
- **A real-cluster test deletes what it creates itself, and never sets process-global state another test relies on.** Delete pods (and anything else it makes in the cluster) directly before asserting, not through smelt's background cleanup (`terminate_pod`, `Sandbox`'s drop queue), which the test process can exit before draining. Don't set `sandbox::MANAGER`, `db`'s pool or any other `OnceLock` from a second test; give the test its own client instead (`open_pod_port_with` takes one for this reason). On SME-42 a new test left two pods behind through the cleanup queue, and setting `MANAGER` broke `test_terminal_lifecycle_end_to_end` with `Kube(Service(Closed))` whenever it ran after the new test finished (see [testing.md](testing.md)).
- **A cluster object a test creates that could block other tests must be harmless if the test is killed at any point,** not only cleaned up on every exit path: a killed process (a CI timeout, a cancel) runs no cleanup at all. Give such an object (a pod labelled like a real conversation's, a claim with a real name) a bounded life, such as a grace period or a deadline, or keep it matching nothing until the moment it has to. On SME-99 the reproduction's own pod, labelled as conversation 1's, was first held by a finalizer, which a killed run would leave Terminating for good, blocking every real-cluster test that uses conversation 1; then a run killed before the delete would leave it running with that label. Both review rounds found one of these.
- **Spikes that create cluster resources use fixed names**, not a PID or timestamp, so a stopped or failed run's leftovers are easy to find and a rerun reuses them. On SME-42 a stopped spike left a pod named after its process id, found only by listing the namespace afterwards.
- Tests live inline with the code they cover: `#[cfg(test)]` blocks in the same `.rs` file
- Use `#[tokio::test]` for async tests
- Use descriptive test names that state the scenario and expected outcome: `test_second_message_does_not_overwrite_title`, not `test_title`
- Use `expect("message")` instead of `unwrap()` so failures are readable
- Test return types can be `-> Result<(), E>` to allow using `?` inside tests

See [testing.md](testing.md) for Rust-specific patterns.

---

## Definition of done

A feature is not finished until **both build targets compile**. Server and web code are gated by different feature flags, so a change that builds for one can silently break the other — a missing `#[cfg(...)]`, a server-only dependency pulled into WASM, or dead code that only one target sees. Run both before considering the work complete or asking for review:

```bash
cargo test  --features server                                                     # server logic + tests
cargo check --no-default-features --features web --target wasm32-unknown-unknown   # WASM frontend
```

(Plain `cargo test` compiles but skips every `server`-gated test, so it proves almost nothing — always pass `--features server`.)

(Real-cluster sandbox tests that create a pod need `scripts/build-sandbox-image.sh` to have run at least once against that cluster — the sandbox pod's image is delivered straight into the cluster's node with no registry involved, so a pod referencing it fails outright (`ImagePullBackOff`) otherwise. Not a compile-time dependency of `cargo test` itself. See [setup.md](setup.md).)

The automated browser tier (`src/browser_tests.rs`, plus `src/webfetch.rs`'s own separate real-browser test — both run by the same command below) covers the sandbox panel (including preview links), the context-usage indicator/detail view/compaction divider, the todo panel, and `webfetch`'s real navigation/SSRF-guard behavior (including the sandbox route) — anything else touching rendering, interaction, or `webfetch` itself still needs a manual pass or its own test. See [testing.md](testing.md#whats-not-covered-yet). **Run it — `scripts/browser-tier` — as part of done for any change touching a covered panel's DOM structure or `src/webfetch.rs`, not just during a later review.** It sat broken for a while after the panel's tabs shipped on `sandbox-visibility`, because nothing re-ran it in between; a deliberate final review caught it, not the change that broke it. The script builds the web bundle first (`dx build --platform web`: the tier serves a separately-built WASM bundle that plain `cargo test` never rebuilds, so a stale one can make a real UI change look broken or still missing; discovered the hard way on `todo-list-tool`), runs the tier against a scratch database it creates and drops, and points a worktree that lacks headless Chrome at the main checkout's (`BROWSER_CHECK_CACHE`). **Don't run the tier against the dev database:** it applies the branch's migrations there, and a branch with a new migration then stops `main` from starting against it until the branch merges (SME-65; on SME-72 the tier was run through a hand-made scratch database for this reason). See [testing.md](testing.md#src-browser_testsrs-automated) for the full reasoning.

**A new browser scenario is a `scenario_*` function plus one `run_scenario` line,** not a numbered block inside a shared test: each scenario makes its own conversations and tabs, so it fails on its own and several branches can add scenarios without agreeing on numbers. On SME-83 and SME-90 two branches each added a "Scenario 26" the same day; SME-59 gave scenarios this shape.

Gate per-target dead code with `#[cfg(feature = "...")]` rather than leaving a warning in the other target.

**Run every setup path the feature suggests for real, once, before the PR.** When the feature hands the user or the model a recipe (an install command, an image, a path, a generated config), each distinct kind of recipe gets one real run: a real-cluster test or a hands-on check. A unit test that asserts the recipe's text only records the same assumption the code made. On SME-35 the catalog's gopls suggestion had a fixture test asserting its command path, and it still never started: the `golang` image put the binary somewhere else. Only a real gopls run in the third review found it.

**Test the edges, not just each piece working on its own.** Before calling a feature done, write a test for how it behaves at each of these, wherever it applies:
- the far side disconnecting or closing mid-operation;
- two calls racing (two opens, an open and a close, a start and a stop);
- a second instance, or reopening after a close — anything left over from the first must not affect the second;
- a slow or failing dependency (a slow page, a refused request, a timeout);
- unusual input — non-ASCII, empty, very long, secrets such as passwords.

For a security boundary, also write down what it does *not* cover, and test the paths around it. On `web-browsing`, most of the 21 bugs three reviews found sat at exactly these edges. One of them was an SSRF guard that only watched one page, while popups, WebSockets and service workers went around it — a gap nobody had written down.

**CI runs all of this automatically on every PR** (`.github/workflows/ci.yml`) — `cargo test --features server`, the WASM `cargo check`, and the browser tier, against the same `docker-compose.yml` stack (Postgres, real k3s cluster) local dev uses. A red CI check means the same thing a red local run does; it doesn't replace running these yourself before pushing, since CI turnaround is much slower than local iteration.

---

## Adding a New Feature (Typical Flow)

1. Add struct to `src/models.rs` (see [models.md](models.md))
2. Add migration: `migrations/YYYYMMDDHHMMSS_description.sql`
3. Add async CRUD functions to `src/db.rs` returning `Result<T, sqlx::Error>`
4. Add server functions to `src/api/` (see [api.md](api.md)) — there's no separate client fetch layer to add, the server function is directly callable from a component
5. Create or extend a page in `src/frontend/pages/`
6. Register a new page in `src/frontend/pages/mod.rs` and, if it needs its own route, `src/frontend/mod.rs`'s `Route` enum

---

## Evolving this process

This document should change as we learn what works. Either party can propose a change at any time — proposals are especially natural after a project wraps up, but don't wait.

**Proactively propose changes** when you notice:
- A step that caused unnecessary friction or delay
- A pattern that worked especially well and is not captured here
- A rule that did not fit the situation

To propose a change: describe it in plain text and explain why. No plan doc needed. Once confirmed, update this file.

Process changes follow the same confirm-before-change rule — propose first, update after the user agrees.

---

## Keeping Linear current

After each project completes:
- Replace the ticket's `## Plan` section with `## What shipped` (including anything changed from the plan, and what's not done)
- Update the **Current state** document if features or architecture changed
- Add or update the project's rows in the **Feature checklist**: what a user can do, and how to check it in the browser
- File anything left undone that's worth doing as its own Backlog ticket, linked as related
- When the project deleted a feature, search for its name in prose too (docs, comments, CSS, prompts, test names), not only its identifiers: the compiler finds the code, nothing finds the sentences. On SME-54, removing `run_async`'s task suite left wording about it in docs and comments that only the code review found.
- Put the ticket id in the PR's title or body
- Review the PR once it's open (see [Code review](#code-review-after-opening-the-pr))
- Once the reviews are done, add `## Retrospective` to the ticket (see below), before the PR merges
- Move the ticket to **Done** once the PR merges

When the user mentions a new idea, create a **Backlog** ticket for it before it is forgotten.

**After a range edit to a Linear ticket or document** (`replace_range`, or replacing a whole section), read it back and compare it against the version from before the edit; sections outside the range should be unchanged. A save's reply can show stale content, so read it again rather than trusting the reply. On SME-32 a `replace_range` on the Current state document silently deleted 11 architecture bullets, restored only because an earlier copy was still in the session. `replace_range`'s `to` anchor is exclusive: end the range at the next section's heading and include that heading in `new_string` (or `replace` the whole old section). Ending it at a section's own last line left that line duplicated on SME-70, SME-49, SME-86 and SME-90.

---

## Retrospective (end of each project)

**Close-out (this section plus "Keeping Linear current" above) is a
gate on merging the project's PR, not a follow-up to do after it merges.**
What shipped, the Current state document and the Feature checklist come
before the PR opens. The retrospective comes after the code review
finishes, since the reviews are often where the most is learned (SME-32's
ten review rounds were, and a retrospective written before them missed
it all).
Do it on the same branch as the implementation, so the close-out commit
rides along in the same PR — not as a separate direct-to-main commit
afterward, which is easy to forget entirely once the PR is merged and
attention has moved to whatever's next. Discovered on `todo-list-tool`:
its close-out was skipped after merge and only caught because the user
noticed the stale plan file still sitting in the repo — checking
"did the last thing get closed out" at the *start* of the next project
relies on remembering to look backward, which is exactly what failed;
gating it at the point the current project's own PR is created doesn't
have that failure mode. The same holds for merging: the retrospective is
the last step before it.

Before considering a project closed, do a short retrospective covering:

- **What worked** that we should keep doing.
- **What caused friction, surprise, or rework** — especially anything discovered
  late that an earlier check would have surfaced.
- **What to change**: concrete proposals to this process, the docs, or the code.

Record it as a short **Retrospective** section in the project's ticket, and say whether a [bug bash](#bug-bash-every-few-projects) is due.
Any resulting process changes follow the [confirm-before-change rule](#evolving-this-process):
propose first, update after the user agrees.

## Code review (after opening the PR)

Every PR gets a code review once it's open, while CI runs: `/code-review <PR number>`. It reads the whole diff for bugs, including security gaps, which a project's own tests are poorly placed to find because they were written with the same assumptions as the code.

1. **Check each finding before reporting it.** Confirm it against the code (and, where it's cheap, reproduce it) so the report says which findings are real.
2. **Report the findings and stop.** Give each one a severity, what goes wrong and a suggested fix, and let the user choose what gets fixed. Fixing changes the PR under review, so it waits for their go-ahead.
3. **Fix on the PR's branch, test-first, one commit per finding.** A failing test first, as in Phase 2; for a finding about a browser's behaviour, a browser-tier scenario shown failing with the fix switched off. A finding in already-merged code gets its own PR (see Rules).
4. **Review again after fixing, twice in all by default.** Fixes change the code, and a second pass looks with fresh eyes. The second round follows fixes: when the first round finds nothing to fix, it's the only one (SME-70). After the second round, only high-severity findings block the merge; report the rest and file each one the user wants as a Backlog ticket, linked as related. **Exception: a third round when the second finds medium-severity issues in a new integration** (a new external system, protocol or process smelt drives). Fix the second round's findings first, then review once more; that's the last. On SME-35 (language servers) the second round found two medium issues, and the third found a catalog suggestion that could never start. SME-32 ran ten rounds, each mostly fixing edges made by the last; a round that keeps finding new edges points to a missing state model (see Phase 1), which another round won't supply.
5. **Record it in the ticket**: a short code-review part in What shipped listing each finding and its fix.
6. **Then write the retrospective** (see [Retrospective](#retrospective-end-of-each-project)), covering the reviews too, and only then merge.

On SME-42, the first review found that both new routes could reach the sandbox agent's command WebSocket (any website could have run commands in the sandbox). The project's own real-cluster tests had used that very port as their test server. The second review, after the fixes, found a stalled POST and cross-site requests to the sandbox. None of the four were caught by the tests, the browser tier or the hands-on check.

## Bug bash (every few projects)

Every three completed projects or so, run a bug bash as its own branch and PR: code sweeps of the areas most recently changed, plus hands-on sweeps of the running app with a real model. Include at least one sweep with two tabs open on the same conversation and a reload mid-reply. Most of what the 2026-09-24 bug bash found sat in flows no single project owned (a second tab, a reload, a background reply, a deleted conversation), so no project's own tests covered them. Every bug bash also runs the whole **Feature checklist** through the web UI (including its busy-session row, with every side panel open at once), marking each row works, broken, model (the model didn't cooperate) or not web-checkable, so "does everything still work" is a list to re-run rather than something to reconstruct. Hands-on checks go through the browser only: no seeding the database or calling APIs directly. Run them against `scripts/check-server`, and drive the browser with `scripts/ui-check/smelt_ui.py` (see [testing.md](testing.md#playwright-preferred)). Write the findings up in the bug bash's ticket, each with a severity and marked confirmed in the UI, confirmed by code, or suspected; stop for the user to choose what gets fixed; then fix them in severity order, one commit per finding, test-first. See [SME-23](https://linear.app/smelt-agent/issue/SME-23) and [SME-40](https://linear.app/smelt-agent/issue/SME-40).

A bug bash ends with its own retrospective, like any project (see Retrospective above): what the sweep caught and why it had been missed, what slowed it down, and what to change in the process or the Feature checklist.

At each project's close-out, check how many projects have completed since the last bug bash, and say so if one is due.

## Design review (every few projects, alternating with bug bashes)

Every six completed projects or so, alternating with bug bashes so one kind of review happens about every three projects, run a design review as its own ticket. The Feature checklist says what should work; a design review asks whether it's good to use. Everything goes through the web UI with a real model, at a laptop width and a phone width, in five passes:

1. **Visual craft**, page by page: type hierarchy, colour and what it means, consistency of repeated elements, spacing, empty states, focus, phone width. Include a **busy session**: every side panel open at once (todos, live browser, sandbox with a terminal), at both widths. Panels are rarely alone in real use; SME-41 reviewed them one at a time and missed that together they squeezed the browser to 100px and the terminal off screen.
2. **Copy**: things named from the user's side, not the system's; buttons that say what happens; errors that say what went wrong and how to fix it.
3. **Information design**: summary before detail, state shown in form and not only in text, interactive things that look interactive.
4. **Workflows**, walked end to end: starting a task, following a long one, stepping in, coming back later, setting up, browsing together, housekeeping.
5. **Information availability**: at each point in those workflows, can the user tell what the model is doing, what it changed, why something failed, how close the conversation is to its limit, and what's running?

Write the findings up in the ticket, each with a category, a severity, evidence (a screenshot or exact steps) and a recommendation, then stop for the user to triage. Small polish the user picks is fixed in one PR, one commit each; anything bigger becomes its own Backlog ticket. Like a bug bash, it ends with a retrospective. See [SME-41](https://linear.app/smelt-agent/issue/SME-41).

At each project's close-out, say whether a design review is due as well.

## Writing idea tickets

Idea tickets describe **what the user will be able to do** or **what problem gets solved** — not how it will be built. Keep them abstract and user-focused.

A good idea ticket answers:
- What can the user do that they cannot do today?
- What problem or friction does this remove?

A good idea ticket does **not** include:
- Implementation approach, data models, or API design
- File names, module structure, or technology choices
- Anything that belongs in a plan

Implementation details belong in the plan, which is written once the idea is approved and work begins. If an idea ticket starts to look like a plan, trim it back.

**Example of what to avoid:** "Add a `tool_calls` table with columns `id`, `message_id`, `name`, `input`, `result` and expose it via a new server function `get_tool_calls()`..."

**Example of the right level:** "Smelt can look things up on the web instead of only answering from what it already knows."
