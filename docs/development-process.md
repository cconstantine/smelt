# Development Process

These rules are mandatory. Follow them in order. Do not skip steps.

---

## Phase 1: Plan

**Do this before writing any code, any tests, or any files other than the plan itself.**

1. Read the user's request carefully
2. Read [projects/state.md](projects/state.md) to understand the current codebase
3. Ask clarifying questions if the request is ambiguous — do not assume
4. Create a branch: `git checkout -b <short-slug>`
5. Write a plan file at `docs/projects/plans/<short-slug>.md` containing:
   - **Branch:** the branch name created in step 4
   - **What** is being built and why
   - **Which files** will be created or modified (be specific)
   - **How** it will be implemented: data model, API shape, UI flow
   - **Open questions or tradeoffs** you are not sure about
6. Show the plan to the user and **stop**
7. **Wait for explicit approval** — a response like "looks good", "yes", or "go ahead"
8. Do not write any implementation code or tests until you receive that approval

If the user requests changes to the plan, update the plan file and show it again. Repeat until approved.

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
- **Verify external crate APIs against the source, not memory.** Read the crate in `~/.cargo/registry/src` (or `cargo doc`) before calling unfamiliar methods, especially for fast-moving or less-documented APIs. Macro-generated or re-exported items don't show up in a `grep` for `fn` and are easy to hallucinate.
- **Spike the riskiest assumption first.** When a planned phase depends on an unproven external or architectural assumption (a runtime, tool, or framework behavior), validate it with a minimal spike before building dependent infrastructure.
- **Bound the boundaries, not every await.** An await only stalls if something it transitively waits on can stall, and that happens where control leaves the process. Put a timeout there — once, at the shared client or wrapper — and let interior awaits inherit it. Don't add timeouts to in-process waits; a hang there is a bug a timeout would hide. At each boundary, also handle every non-success terminal state explicitly (cancelled, superseded, truncated — not just `Err`), and add `tracing` at the seam so a stall identifies itself from a console read.
- **Two `async fn`s that call each other (directly or through a longer cycle) defeat rustc's `Send`-auto-trait inference**, surfacing as a cryptic `cannot satisfy \`impl Future: Send\`` error pointing at an unrelated line, not at the cycle itself. Break it by making at least one side return a boxed, type-erased future (`Pin<Box<dyn Future<Output = T> + Send>>`) instead of relying on `async fn` sugar's opaque return type. `api::chat::run_turn` and `anthropic::tools::execute` are a concrete instance of this shape (a turn loop that dispatches tools, one of which can itself trigger another turn) — expect the same pattern anywhere a conversation loop hands control to a tool/proxy that can call back into it, which a real (non-throwaway) tool-use feature will.
- **Surface fallback outcomes on user-visible flows.** When a feature degrades gracefully — a parser returns `None`, a lookup misses, an optional enrichment fails — the UI must say what happened. A silent fallback is indistinguishable from the code not running at all, both to the user and to whoever debugs their report. Best-effort is fine; invisible is not.
- **Fixture real artifacts — don't re-synthesize them.** When a bug is reproduced from a real artifact (an API response, a wire capture, a malformed input), check the artifact's actual bytes in as the regression fixture. A hand-built synthetic fixture encodes the same assumptions that produced the bug.
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

There is no automated browser tier yet, so anything touching rendering or interaction needs a manual pass too — see [testing.md](testing.md#whats-not-covered-yet).

Gate per-target dead code with `#[cfg(feature = "...")]` rather than leaving a warning in the other target.

---

## Adding a New Feature (Typical Flow)

1. Add struct to `src/models.rs` (see [models.md](models.md))
2. Add migration: `migrations/YYYYMMDDHHMMSS_description.sql` (see [migrations.md](migrations.md))
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

## Keeping project docs current

After each project completes:
- Add a file to [projects/completed/](projects/completed/) named `YYYYMMDD-short-slug.md`
- Update [projects/state.md](projects/state.md) if features or architecture changed
- Remove the idea file from [projects/ideas/](projects/ideas/) if it originated there
- Remove the plan file from [projects/plans/](projects/plans/) if one was created
- Run a retrospective (see below)

When the user mentions a new idea, add a file to [projects/ideas/](projects/ideas/) before it is forgotten.

---

## Retrospective (end of each project)

Before considering a project closed, do a short retrospective covering:

- **What worked** that we should keep doing.
- **What caused friction, surprise, or rework** — especially anything discovered
  late that an earlier check would have surfaced.
- **What to change**: concrete proposals to this process, the docs, or the code.

Record it as a short **Retrospective** section in the project's `completed/` doc.
Any resulting process changes follow the [confirm-before-change rule](#evolving-this-process):
propose first, update after the user agrees.

## Writing idea files

Idea files describe **what the user will be able to do** or **what problem gets solved** — not how it will be built. Keep them abstract and user-focused.

A good idea file answers:
- What can the user do that they cannot do today?
- What problem or friction does this remove?

A good idea file does **not** include:
- Implementation approach, data models, or API design
- File names, module structure, or technology choices
- Anything that belongs in a plan

Implementation details belong in the plan, which is written once the idea is approved and work begins. If an idea file starts to look like a plan, trim it back.

**Example of what to avoid:** "Add a `tool_calls` table with columns `id`, `message_id`, `name`, `input`, `result` and expose it via a new server function `get_tool_calls()`..."

**Example of the right level:** "Smelt can look things up on the web instead of only answering from what it already knows."
