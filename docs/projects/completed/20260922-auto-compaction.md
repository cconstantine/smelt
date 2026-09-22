# Auto-compaction and context visibility

**Branch:** `auto-compaction` · **Idea:** `projects/ideas/auto-compaction.md` (removed) · **Plan:** `projects/plans/auto-compaction.md` (removed)

## What shipped

Two connected capabilities, exactly as scoped:

- **Context visibility**: an always-visible indicator (a percent/progress
  bar in the normal chat UI) showing how full the model's context window
  is, built on real `usage` numbers from Anthropic's own API responses —
  `message_start`/`message_delta`'s `usage` fields were previously parsed
  by nothing in this codebase at all. Clicking it opens a detail view:
  system prompt (currently always "none set" — a real system prompt
  remains `coding-session.md`'s own separate open item), every available
  tool's full definition, message count, the same usage numbers, and a
  visual segmented-bar breakdown of the context window by category
  (input/output/cache-creation/cache-read tokens, plus free space) — built
  following the dataviz skill's procedure, using its validated default
  categorical palette in fixed order.
- **Auto-compaction**: before a request would be sent, its real size is
  estimated (the last real response's own usage, plus a cheap chars/4
  estimate — empirically spiked against the real gateway at ~3.8 real
  chars/token, close enough not to revisit — of whatever's new since
  then) against a reserved ceiling (context window minus the model's own
  reply budget and a safety buffer), mirroring opencode's own mechanism.
  Once crossed, a separate, tools-less Anthropic call summarizes
  everything so far — explicitly handed every currently-live sandbox
  pod/terminal/background-task id so it doesn't have to notice and
  preserve them unprompted — and the result is persisted as a new
  `ContentBlock::CompactionSummary`, sandwiched between two structural
  `ContentBlock::CompactionPlaceholder` messages (Anthropic requires
  `messages` to start with `user` and strictly alternate; a single summary
  block can't satisfy both "starts with user" and "ends with something to
  respond to" on its own). Nothing already persisted is ever rewritten or
  deleted — only what gets *replayed* to Anthropic on later turns changes,
  via `history_for_request`, which also guarantees a `tool_use`/
  `tool_result` pair is never split across a compaction boundary, and that
  only the *latest* compaction's boundary applies if a conversation
  compacts more than once. A failed summarization call fails the whole
  turn loudly rather than risking the oversized request compaction exists
  to prevent. The compaction event itself renders as a distinct, collapsed
  divider in the transcript — never an ordinary chat bubble, and the two
  structural placeholder messages render as nothing at all.

**Verification:** 250 `cargo test --features server` tests passing (up
from 216 at the start of this project), both build targets clean, the
extended automated browser tier (now 6 scenarios, up from 4, in the one
existing end-to-end test — including the indicator, detail view, and
compaction divider) passing against a real headless Chrome, and multiple
real, non-mocked verification passes against the live Anthropic-compatible
gateway configured in this environment: real usage numbers parsed
correctly from the real wire format, and a full compaction round trip
(seeded near the ceiling, driven through the real running app) producing
a real summarization call, a correctly rendered divider, a correctly
shrunk context-usage indicator afterward, and a coherent real model reply
to the post-compaction continuation prompt.

## Retrospective

**What worked:**
- **Reasoning through Anthropic's own protocol constraints on paper before
  writing code caught a real, would-have-shipped-broken bug.** The
  original plan for representing a compaction implied a single summary
  message; working through what `messages` actually has to look like
  (must start with `user`, must strictly alternate) before implementing
  it surfaced that no single message can satisfy both "starts with user"
  and "ends with something to respond to" — leading to the three-message
  `compaction_messages` design (`user` placeholder → `assistant` summary →
  `user` continuation) instead. Caught by deliberate reasoning, not by a
  failing test — worth naming alongside `development-process.md`'s
  existing "verify external crate APIs against source" rule as the same
  discipline applied to a wire protocol's own shape, not just a library's.
- **Real, non-mocked verification at multiple points**, not just mocks:
  parsing real `message_start`/`message_delta` usage against the actual
  gateway confirmed the wire-format assumptions from spec-reading; a
  seeded near-ceiling conversation driven through the real running app
  confirmed the entire compaction round trip end-to-end, including an
  authentic, sensible model reply to the post-compaction continuation
  prompt; and the token-estimation heuristic (`chars/4`) got a real
  empirical data point (~3.8 actual chars/token) instead of staying a pure
  assumption, using a reusable method (diff two real `usage` reads,
  subtract the earlier one's `output_tokens`).
- **Splitting pure/testable logic from async glue** kept almost the whole
  feature strict-TDD-able (`should_compact`, `estimate_tokens`,
  `is_safe_compaction_boundary`, `history_for_request`,
  `compaction_messages`, `context_usage_breakdown`/`_percent`), while the
  DB/API-touching glue (`compact_conversation`, `describe_live_state`)
  followed this codebase's established "mechanical, verified by
  integration test" precedent instead of a strict red-green cycle.
- **Extending the one existing browser-test tier instead of starting a
  new mechanism** — `docs/testing.md` had explicitly flagged it as "worth
  extending once another feature has a similar need," and this was that
  moment. Reused its established discipline (bypass the model entirely,
  seed state directly) without needing to invent anything new, and caught
  a real dependency issue along the way: `describe_live_state` initially
  called `sandbox::list_pods`/`list_terminals`, which panic if the
  process-global sandbox manager was never initialized — true of most
  tests. Switching to the lower-level `db::` row queries directly (which
  is all a summarization prompt actually needs — not live connection
  status) fixed it, and was a better design regardless of the test.

**What caused friction, surprise, or rework:**
- **A type gated server-only in the immediately preceding PR needed
  un-gating.** `ToolDefinition` had just been moved behind
  `#[cfg(feature = "server")]` specifically because "the web build never
  touches it" — the context-detail view's tool list is exactly a case
  where the web build now does. Not a mistake in either piece of work,
  just sequential discovery: a completed project's own stated rationale
  can stop holding once the very next feature has a genuinely new
  requirement.
- **A real, silent correctness gap in already-shipped code, caught by a
  later, unrelated feature.** The first-shipped `context_usage_percent`
  quietly excluded `cache_creation_input_tokens`/`cache_read_input_tokens`
  from "how full is it" — not caught until building the *detailed*
  breakdown meter forced a full accounting of every usage category side
  by side. Fixed with a shared `context_usage_breakdown` helper and a
  regression test proving the old behavior was wrong. Worth remembering:
  a narrow feature's own tests can all pass while still quietly omitting
  a whole category the feature's next iteration happens to expose.
- **`run_turn_bounded`'s in-memory `history` accumulator had to be
  removed entirely**, not just extended, to make compaction's
  boundary-based history rewriting possible — replaced with a fresh
  `db::list_messages` fetch plus `history_for_request` translation on
  every request. Bigger than anticipated going in, though it also
  simplified `drain_unnotified_terminal_commands`'s own signature as a
  side effect (one less thing to keep in sync by hand).

**What to change:**
- Confirmed and applied: `development-process.md`'s "Rules" section now
  names *wire-protocol structural constraints* (alternation rules,
  required-first-role, and similar shape requirements) as their own
  category worth verifying against the real spec before writing code —
  right alongside the existing "verify external crate APIs against
  source" rule, since this was the same discipline applied to a protocol
  rather than a library.
