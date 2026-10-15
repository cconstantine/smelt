# Definition of done
Part of the [development process](../development-process.md).


A feature is not finished until **both build targets compile**. Server and web code are gated by different feature flags, so a change that builds for one can silently break the other — a missing `#[cfg(...)]`, a server-only dependency pulled into WASM, or dead code that only one target sees. Run both before considering the work complete or asking for review:

```bash
cargo test  --features server                                                     # server logic + tests
cargo check --no-default-features --features web --target wasm32-unknown-unknown   # WASM frontend
```

(Plain `cargo test` compiles but skips every `server`-gated test, so it proves almost nothing — always pass `--features server`.)

(Real-cluster sandbox tests that create a pod need `scripts/build-sandbox-image.sh` to have run against that cluster for the working tree's agent sources — `scripts/check.sh` runs them against `smelt-sandbox:src-<hash>`, and `scripts/cluster-doctor` says when it's missing (SME-102) — the sandbox pod's image is delivered straight into the cluster's node with no registry involved, so a pod referencing it fails outright (`ImagePullBackOff`) otherwise. Not a compile-time dependency of `cargo test` itself. See [setup.md](../setup.md).)

The automated browser tier (`src/browser_tests/`, plus `src/webfetch.rs`'s own separate real-browser test — both run by the same command below) covers the sandbox panel (including preview links), the context-usage indicator/detail view/compaction divider, the todo panel, and `webfetch`'s real navigation/SSRF-guard behavior (including the sandbox route) — anything else touching rendering, interaction, or `webfetch` itself still needs a manual pass or its own test. See [testing.md](../testing.md#whats-not-covered-yet). **Run it — `scripts/browser-tier` — as part of done for any change touching a covered panel's DOM structure or `src/webfetch.rs`, not just during a later review.** It sat broken for a while after the panel's tabs shipped on `sandbox-visibility`, because nothing re-ran it in between; a deliberate final review caught it, not the change that broke it. The script builds the web bundle first (`dx build --platform web`: the tier serves a separately-built WASM bundle that plain `cargo test` never rebuilds, so a stale one can make a real UI change look broken or still missing; discovered the hard way on `todo-list-tool`), runs the tier against a scratch database it creates and drops, and points a worktree that lacks headless Chrome at the main checkout's (`BROWSER_CHECK_CACHE`). **Don't run the tier against the dev database:** it applies the branch's migrations there, and a branch with a new migration then stops `main` from starting against it until the branch merges (SME-65; on SME-72 the tier was run through a hand-made scratch database for this reason). See [testing/browser-tier.md](../testing/browser-tier.md#srcbrowser_tests-automated) for the full reasoning.

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

## Adding a New Feature (Typical Flow)

1. Add struct to `src/models.rs` (see [models.md](../models.md))
2. Add migration: `migrations/YYYYMMDDHHMMSS_description.sql`
3. Add async CRUD functions to the module for their tables under `src/db/` (re-exported by `src/db/mod.rs`, so callers write `db::name`), returning `Result<T, sqlx::Error>`
4. Add server functions to `src/api/` (see [api.md](../api.md)) — there's no separate client fetch layer to add, the server function is directly callable from a component
5. Create or extend a page in `src/frontend/pages/`
6. Register a new page in `src/frontend/pages/mod.rs` and, if it needs its own route, `src/frontend/mod.rs`'s `Route` enum
