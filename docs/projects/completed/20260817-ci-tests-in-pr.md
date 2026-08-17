# CI: run the full test suite on every PR

**Branch:** `ci-tests-in-pr` · **Plan:** `projects/plans/ci-tests-in-pr.md` (removed)

Not from an idea file — the user's first step in a broader "improve the
development process" effort: get tests running automatically on every PR,
since none of this repo's tests had ever run anywhere but a developer's own
machine.

## What shipped

**`.github/workflows/ci.yml`**, running on every PR (and push to `main`):
the full `cargo test --features server` suite (Postgres + real-cluster k3s
sandbox tests), the WASM `cargo check`, and the automated browser test tier
— every test defined in the repo, not a subset, reusing the exact same
`docker-compose.yml` stack (Postgres, Docker-in-Docker, k3s) local dev
already depends on rather than inventing a parallel CI-only environment.
`docker compose run` inside the `smelt` service (not the bare runner) for
every command, since the kubeconfig's server address only resolves inside
that network and the browser tier needs the dev image's baked-in
Playwright/Chromium anyway.

No secrets needed — every test that talks to "Anthropic" uses a hardcoded
`test-key` against a mock upstream, never the real API.

## The first real run and what it actually found

Nothing about this branch's first green run was obvious in advance — seven
iterations, each fixing one real, previously-invisible problem:

1. **Missing build step.** `src/sandbox.rs`'s `include_bytes!` embeds a
   pre-built `sandbox_agent` binary; `scripts/build-sandbox-agent.sh` has to
   run first, and that requirement had never made it into `setup.md` (the
   doc that named it — the `sandbox-terminal` plan — was removed per the
   project's own completed-project cleanup step, and nothing ported the
   requirement forward). Added the CI step and documented it in both
   `setup.md` and `development-process.md`'s Definition of done.

2. **Sandbox tests timing out — three attempts before the real cause.**
   Two `sandbox::` tests reliably hit `SandboxError::Timeout` in CI only.
   First guess: raise `RUNNING_WAIT_TIMEOUT` (30s → 120s, made
   configurable via `SANDBOX_RUNNING_WAIT_TIMEOUT_SECS`, matching the
   existing `SANDBOX_MEMORY_LIMIT`/`SANDBOX_CPU_LIMIT` env-var pattern).
   Didn't fix it — same two tests, same failure, now waiting the full
   120s first. Second guess, after reproducing a *plausible-looking but
   wrong* CPU-contention failure locally with a throwaway diagnostic
   (filler pods reserving CPU before a normal create): lower
   `SANDBOX_CPU_LIMIT` to `250m`. Also didn't fix it — identical failure,
   at the identical elapsed time, regardless of CPU size, which in
   hindsight should have been the tell that CPU was never the actual
   constraint. Added a diagnostics-on-failure CI step (`kubectl get/
   describe pods`, events) instead of guessing a third time, and it found
   the real cause on the very next run: `FailedScheduling … Insufficient
   memory` — every test pod requested the production `8Gi` default, and
   the runner's node doesn't have that much allocatable memory to spare
   for more than one concurrent pod. Lowered memory the same way
   (`128Mi` in `SandboxManager::create`'s test call sites, `SANDBOX_
   MEMORY_LIMIT=128Mi` for `create_pod`'s tests) and it was gone for good.

3. **`browser-check/setup.sh`'s apt fetch — another two-attempt chase.**
   Failed with a confusing `curl: no URL specified` (an empty `apt-get
   --print-uris` result feeding `xargs` no arguments). Didn't reproduce
   locally. First fix: make the failure loud instead of confusing. Second
   run: same empty result, but now clearly *not* a one-off flake. Made the
   `apt-get update` call itself verbose (it had been silently succeeding
   under `-qq`) and dumped the fetched index — confirmed the update
   fetched a real, complete package index, ruling out network failure.
   Third run: added a non-quiet rerun of the failing install specifically
   to show apt's own reasoning, and it explained itself immediately —
   `libnspr4 is already the newest version` (and all twelve others). The
   CI-built image's `playwright install-deps chromium` Dockerfile step
   already installs every library this script exists to fetch; the old
   guard (`[ ! -f "$LIBDIR/libnspr4.so" ]`) only ever checked our own
   empty cache directory, never whether the system already satisfied the
   real requirement. Replaced it with an `ldd`-based check against the
   actual binary (LD_LIBRARY_PATH-aware, so a previously-fetched cache
   still counts, preserving the script's documented idempotency).

4. **A genuine, pre-existing source bug** — `src/browser_tests.rs`'s one
   `sandbox::create_pod` call site still passed 2 arguments; the function
   had grown `memory_limit`/`cpu_limit` params at some point and this call
   site, in a feature combination nothing had ever compiled before this
   branch, was never updated. A one-line fix (`None, None`, matching every
   other call site in the codebase), verified not just by compiling but by
   actually running the normally-`#[ignore]`d browser test locally end to
   end.

**Verification:** every fix from #2 onward was reproduced or verified
locally before pushing — including a throwaway diagnostic test (written,
run, and deleted) that deliberately reserved cluster CPU to test the first
theory, and a full local run of `scripts/browser-check/setup.sh` → `dx
build --platform web` → the real (not just compiled) browser test for the
last fix. The one exception is the empty-apt-URLs failure itself, which
never reproduced outside CI at all — diagnosed entirely from progressively
more detailed CI logs instead.

## Retrospective

**What worked:**
- Reproducing locally before trying a fix (the user's explicit ask,
  starting from the memory-vs-CPU confusion) is what turned a plausible
  but wrong theory into a *known-wrong* theory instead of a shipped
  no-op fix — the local CPU-pressure repro genuinely did reproduce a
  `Timeout`, just not *the* `Timeout` CI was hitting. Two different bugs
  can share a symptom; only checking whether the specific failure's
  *evidence* (the actual scheduler event) matches the theory would have
  caught this sooner than a second blind attempt did.
- Adding a diagnostics-on-failure CI step, instead of a third guess, paid
  for itself on the very next run — `FailedScheduling: Insufficient
  memory` was completely unambiguous, versus two rounds of inferring a
  cause from nothing but "still times out."
- A background `gh run watch <id> --exit-status`, re-armed after every
  push, turned "keep checking CI" into a real event-driven wait instead
  of manual polling — worth doing by default for any multi-iteration CI
  debugging loop like this one.

**What caused friction, surprise, or rework:**
- **A masked exit code from my own command wasted one full check cycle.**
  `gh run watch <id> --exit-status; echo "RUN_CONCLUSION_EXIT=$?"` reports
  the *echo's* exit code (always 0) as the background task's own
  completion status, not the run's real conclusion — the notification
  said "exit code 0" for a run that had actually failed, caught only by
  independently re-checking `gh run view`. Don't put anything after a
  command whose own exit code the background-task notification needs to
  carry.
- **Two "fix" cycles (timeout, then CPU) were spent on the same wrong
  theory before checking what the scheduler itself said.** Both fixes
  were reasonable-sounding and both failed identically — the second
  failure, at the same elapsed time regardless of a 4x-smaller CPU
  request, was itself evidence the theory was wrong, but that signal
  wasn't acted on until a third, different kind of check (real cluster
  diagnostics, not another local repro) was added.

**What to change (proposed, not yet applied to `development-process.md`):**
- When a real-environment-only failure resists two consecutive fixes built
  on the same theory, stop attempting a third variation of that theory —
  add direct observability into the real environment (here: `kubectl
  describe`/events) before trying anything else. The pattern "same
  failure, same timing, different magnitude of the thing I changed" is
  itself a signal the change targeted the wrong mechanism.
