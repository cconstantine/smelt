# CI: run tests in a PR

**Branch:** `ci-tests-in-pr`

## What

Add a GitHub Actions workflow that runs on every pull request (and pushes to
`main`) and runs **every defined test in the repo**, not just the default
`cargo test` set:

- `cargo test --features server` — the real test suite, including the
  Postgres-backed `#[sqlx::test]` tests and the real-cluster `src/sandbox.rs`
  k3s integration tests. Nothing is skipped or filtered — CI gets the same
  coverage a local `docker compose`-based run gets.
- `cargo check --no-default-features --features web --target
  wasm32-unknown-unknown` — the WASM/client compile check (existing local
  [Definition of done](../../development-process.md#definition-of-done)).
- `dx build --platform web` followed by
  `cargo test --features "server browser-test" -- --ignored --test-threads=1`
  — the automated browser tier (`src/browser_tests.rs`), `#[ignore]`d by
  default locally but explicitly included here since the goal is "all
  defined tests run in CI." Needs the Playwright/Chromium install the
  Dockerfile already bakes into the dev image (see "Runner and services"
  below) plus `scripts/browser-check/setup.sh`'s chrome-headless-shell
  download and a pre-built `dx build --platform web` output, per
  `testing.md`.

No `ANTHROPIC_API_KEY`/secrets are needed — every test that talks to
"Anthropic" uses `test-key` against a mock upstream (`ANTHROPIC_BASE_URL`
repointed at a throwaway local Axum server), not the real API.

## Why

No CI exists in this repo today (confirmed: no `.github/workflows`
directory). Tests only run when someone remembers to run them locally.
Gating PRs on a real test run is the first item in a broader "improve the
dev process" effort the user wants to start.

## Which files

- **New:** `.github/workflows/ci.yml` — the workflow itself.
- No application code changes.

## How

### Runner and services

GitHub's hosted `ubuntu-latest` runners support privileged Docker containers,
so the existing `docker-compose.yml` stack (`postgres`, `docker` DinD
sidecar, `k3s`, `k3s-bootstrap`) can be reused mostly as-is rather than
hand-rolling a parallel CI-only service definition — same setup local dev
already relies on, so there's only one stack to keep working, not two that
can drift apart.

**Decision: run `cargo test` inside the `smelt` compose service itself**
(`docker compose run smelt ...`), not on the bare runner. This was an open
question when only `cargo test --features server` was in scope; it's settled
now that the browser tier is in scope too — that tier needs the
Playwright/Chromium install the `Dockerfile`'s `dev` target already bakes
in, so building that image is required regardless, and reusing it means
`docker-compose.yml`'s existing `DATABASE_URL`/`KUBECONFIG`/`DOCKER_HOST`
wiring works verbatim with no new port-publishing or volume-reading logic
to invent and keep in sync with local dev.

Plan:

1. Checkout the repo.
2. `docker compose build smelt` — build the dev image (cached via GitHub
   Actions' `docker/build-push-action` + `actions/cache`, or a registry
   cache, so this isn't paid in full on every run).
3. `docker compose up -d postgres docker k3s`, then wait for `k3s-bootstrap`
   to exit 0 (`docker compose up --wait k3s-bootstrap` or an explicit poll)
   — it writes the kubeconfig the sandbox tests need, the same
   `service_completed_successfully` dependency `smelt` itself waits on.
4. `docker compose run --rm smelt cargo test --features server` — the real
   test suite, using the container's already-correct `DATABASE_URL`/
   `KUBECONFIG`/`DOCKER_HOST` env from `docker-compose.yml`, no overrides
   needed.
5. `docker compose run --rm smelt cargo check --no-default-features
   --features web --target wasm32-unknown-unknown`.
6. Browser tier:
   - `docker compose run --rm smelt scripts/browser-check/setup.sh` —
     downloads chrome-headless-shell into the container (per-run cost;
     revisit caching if this proves slow).
   - `docker compose run --rm smelt dx build --platform web`.
   - `docker compose run --rm smelt cargo test --features "server
     browser-test" -- --ignored --test-threads=1`.
7. `docker compose down -v` in a final/always-run step to avoid leaking
   runner-side state between jobs (each job gets a fresh runner anyway, but
   cheap to be explicit).

### Open questions / tradeoffs

- **Runtime.** k3s boot + a full dev-image build + Playwright/Chromium setup
  + `dx build` is not fast — this is now the slowest possible version of
  this workflow, running literally every defined test. No specific time
  budget was given; worth a first real run to see where it lands before
  deciding whether to trim anything (image layer caching is the biggest
  lever if it's too slow).
- **Concurrency.** Not addressing PR-to-PR runner contention/cancellation
  (`concurrency:` groups) in this pass — can add if it becomes a problem.
- **`--test-threads=1` for the browser tier** mirrors `testing.md`'s
  documented invocation exactly (single browser test today, but the flag is
  there in the docs so kept as-is).

## Definition of done for this change

- A PR opened against this branch shows a CI check running and passing
  (or a deliberately-broken test showing it failing, then fixed, if we want
  to prove the gate actually gates).
- `docs/setup.md` and/or `docs/development-process.md` updated to mention
  the CI gate exists, so it's discoverable the same way the local
  Definition of done is.
