# SME-73 plan: a turn that overflows the model's context compacts and carries on instead of failing

**Branch:** `sme-73-overflow-compacts`, from `origin/main` `59cd573` (2026-11-03), pushed; this file is its first commit and merges with the PR (`docs/plans/sme-343.md`'s precedent; a sandbox seat, one plain clone — rules.md). The 2026-10-09 branch of this name was deleted leaving no plan file.

**Decided (Claude, 2026-11-03, ticket comment):** a learned window is kept as the model's **Context window** setting (`provider_models.context_window`, option (a)); Ollama's quiet truncation **is** handled here (`/api/ps` loaded-window check). No questions are open.

## What the re-verification against `59cd573` changed

Written at base `535f2a9`, last checked at `e6f84bb` (2026-10-09). Every symbol it named still exists; what landed or moved since:

- **SME-137 (OTEL) Done** — the steps are rewritten against its spans (§ Spans).
- **SME-343 Done** — the turn's calls ride the request slot the turn holds (§ Slots); SME-354 B6 added yielding it across a tool pass.
- **SME-178 Done** — `message_delta`'s input/cache counts are already taken (`stream/meter.rs`); the old plan's meter item is **on main**; Question 2's scope shrinks to the `/api/ps` check.
- **SME-354 / SME-123 DR3** — a failed user turn saves a failure notice and ends `TurnFailure::Noted { reason }` (`run.rs`'s `note_failure`); the kept reconnect error is `TURN_FAILED_NOTED`.
- **Symbol moves:** `is_ollama_thinking_tool_call_corruption` and `TOOL_CALL_PARSE_RETRIES` live in `src/turn/call.rs` now, around `call_model`. `stream_anthropic_message` is the `#[cfg(test)]` wrapper; the production entry is `stream_anthropic_message_metered` (callers below). `TurnModel`, `resolve_turn_model` and `context_window()` live in `src/providers/server.rs`. `src/db.rs` split: the learn-write goes in `src/db/providers.rs` (next to `set_provider_model_reported`). `src/browser_tests.rs` and `src/frontend/pages/chat/tests.rs` are directories; the old plan's `src/turn/overflow_tests.rs` → **`src/turn/tests/overflow.rs`**. `parse_ollama_ps` and the `/api/ps` fetch pattern are in `src/anthropic/models.rs` (`ollama_details` uses them at listing).

## What and why

Today smelt compacts only ahead of time, against the window it believes the model has (`should_compact`, `src/turn/compaction.rs`). When that belief is too big (a local server's window is smaller than assumed; a gateway model's size unknown), the provider refuses the request, the turn fails with "model provider error 400 …", and the user has to intervene. After this change smelt recognises a refusal that means "this didn't fit", records the real window when the refusal names it, makes the request fit and sends it once more: budget too big lowers the budget; conversation too big compacts first; still-doesn't-fit fails with a message that says why and what to change. opencode does the same; smelt keeps its proactive check.

## How each provider kind reports a request that doesn't fit

| Kind | Refused request | Reply that runs into the window | Names the window? |
| --- | --- | --- | --- |
| **Anthropic** | **400** `invalid_request_error`: "prompt is too long: A tokens > W maximum" (every model; body `{"type":"error","error":{…},"request_id":…}`); on pre-Claude-4.5 models input + `max_tokens` over is also a 400: `input length and max_tokens exceed context limit: A + M > W` (worded per hermes-agent PR #126600). **413** `request_too_large` is >32 MB of *bytes* — never a token overflow. | Claude 4.5+ accept input + `max_tokens` over the window and stop with `stop_reason: "model_context_window_exceeded"`. | Yes: "A tokens > W maximum", or "A + M > W". |
| **Ollama** | **Nothing, by default.** Its `/v1/messages` shim builds an `api.ChatRequest` without `truncate`; truncation then defaults on (`server/routes.go`); `chatPrompt` drops the oldest messages until the prompt fits `num_ctx`, keeping system messages and the last; an oversized last message is cut by context shift. Without shift: **400** `invalid_request_error`, "the prompt is longer than the context length currently available to the model; shorten the prompt, …" (`llm/llama_server.go`), status kept by the shim's error writer. | It context-shifts and carries on (quietly truncated). Shift off: `done_reason` `length` maps to `max_tokens`. | No — but `/api/ps` reports the loaded `context_length`. |
| **llama.cpp** | **400**, `{"error":{"code":400,"type":"exceed_context_size_error","message":"request (A tokens) exceeds the available context size (W tokens), try increasing it","n_prompt_tokens":A,"n_ctx":W}}` (`server-context.cpp`); the other wording is "input (A tokens) is larger than the max context size (W tokens). skipping". A streamed request's first error comes back as this plain response with its status. `max_tokens` isn't checked against the window. | Context shift is off by default (`common/common.h`), so generation stops when the slot fills — and anything but a stop word is `stop_reason: "max_tokens"` (`to_json_anthropic`): it looks like the budget though fewer tokens were written. | Yes, as `n_ctx`. |
| **Other** (gateways) | **vLLM: 400**, "This model's maximum context length is W tokens. However, you requested M output tokens and your prompt contains A input tokens, …" (`vllm/renderers/params.py`), Anthropic-shaped (`api_router.py`); checks input **plus** `max_tokens`. An older wording, covered by the same pattern, is in claude-code-router #1799. **LiteLLM: 400**, prefixed `litellm.ContextWindowExceededError:`. **OpenRouter: 400**, typed `context_length_exceeded`. A gateway in front of Claude passes Claude's errors and stop reasons through. **Some hide the real error**: claude-code-router answers "All target providers failed." for vLLM's 400 (#1799). | `model_context_window_exceeded` behind Claude, else `max_tokens`. | Usually, in the message. |

Sources: Anthropic's docs — errors, context-windows and streaming pages (an error can also arrive as an SSE `error` event; `message_delta` usage is cumulative — since SME-178). Source read at: Ollama `c2b7368`, llama.cpp `de7fa0a`, vLLM `ab905a8`, LiteLLM `577d74c`, opencode `3884062`.

**opencode** (`provider-error.ts`, `session/processor.ts`, `compaction.ts`): an error matching one of ~30 patterns and no rate-limit exclusion marks the session as needing compaction and the turn carries on; a compaction that overflows too stops with a "too large to compact" error. smelt follows that shape, stricter: hermes-agent PR #126600 saved the input count (188240) as the window from an "A + M > W" message — the mistake this classifier must not make.

**Probe log (2026-10-08, the user's llama-server; seats never send prompts to it — the mock scenario is the proof).** `/props` → `flash-next`, 1 slot, `n_ctx` 262,144; empty-messages `POST /v1/messages` → **500** `{"error":{code,message,type}}` (no top-level `type` — `provider_error_message` already reads this shape); a `max_tokens`-300000 probe queued behind the one busy slot and timed out unanswered. From source: a real refusal needs a prompt over `n_ctx` — so tests use bodies built from the source above.

## Telling an overflow apart from other errors

New `src/anthropic/overflow.rs`: `classify(status: Option<u16>, body: &str) -> Option<Overflow>`, `Overflow { window: Option<u32>, input_tokens: Option<u64>, message: String }`.

- **Status gate.** Only a 400 or 413, or an SSE `error` event (no status). 429, 5xx, 529 are never an overflow (the transient statuses are already retried). Thinking-binding refusals are retried first, as today (`next_binding`), and none of their wordings match.
- **Typed first** (whatever the message): `error.type` `exceed_context_size_error` (llama.cpp) or `request_too_large` (Anthropic 413); `error.metadata.error_type` `context_length_exceeded` (OpenRouter).
- **Then wording,** case-insensitive, each pattern from a cited body: "prompt is too long", "exceed context limit", "exceeds the available context size", "larger than the max context size", "maximum context length", "longer than the context length", "contextwindowexceedederror", "context_length_exceeded".
- **Numbers only from known shapes, never a bare number:** llama.cpp's `n_ctx`/`n_prompt_tokens`; Anthropic's "A tokens > W maximum" and "A + M > W" (window is W, not A); vLLM's "maximum context length is W"/"of W", "prompt contains A input tokens"/"A tokens from the input messages". A window under 1,024 is ignored.

`stream_anthropic_message_metered` returns `Result<StreamedTurn, ModelError>` where `ModelError` is `Overflow(Overflow)` or `Other(String)`; its `Display` is today's text, so every other error reads unchanged (the `#[cfg(test)]` wrapper keeps `Result<…, String>` by mapping `into()`; `call_model`'s corruption check reads `e.message()`). Its two production callers: `turn/call.rs`'s `call_model`, `turn/compaction.rs`'s `compact`. Each cited body becomes a unit test in `overflow.rs`, with must-not-count cases: a 429 "Too many tokens", a 400 binding refusal, a 400 `cache_control` refusal, the 500 Jinja error.

## What happens next

For each model request in a turn (`call_model` in `run_turn_steps`; the first and every tool round), at most **one recovery**:

1. **Learn.** If the refusal names a window W smaller than the turn's, the turn updates its own `TurnModel.context_window` copy (the `resolved_model` snapshot, so `should_compact`, `build_request`/`reply_budget` and `recorder.finish`'s `ContextUsageUpdate` all use it) and W is saved for the model (§ What smelt learns).
2. **Budget only, when the input fits.** The refusal may name the input A (vLLM, Anthropic's older wording); otherwise A is smelt's projection. If A + `MIN_REPLY_TOKENS` + `COMPACTION_SAFETY_BUFFER` fits in W, only the reply budget was too big: rebuild from the updated `TurnModel` (a fresh `build_request`, which recomputes `reply_budget`) and send the same history again without compacting.
3. **Compact, when it doesn't.** If the input doesn't fit, or no window was named, run `compact_conversation` with the window now known, rebuild the request, and send it once more. With no window named, this turn's compaction and retry assume three quarters of the refused request's size (A if reported, else `estimate_request_tokens`); that bound isn't saved.
4. **Can't help.** If the system prompt and tool definitions alone (`estimate_request_tokens` with no messages) leave less than `MIN_REPLY_TOKENS` + `COMPACTION_SAFETY_BUFFER` of W, don't compact — fail at once (wording below).
5. **Refused again.** A retry refused as an overflow too fails the turn; a window that refusal names is still learned, so the next turn starts right.

**A compaction that overflows** (the recovery's compaction or the proactive one) gets the same treatment once, inside `compact_conversation`: learn W, summarise again with `compaction_transcript_budget(W)` — or half the refused transcript when no window was named. A second refusal fails the turn: "Couldn't compact the conversation: even the summary request didn't fit `<model>`'s context window (W tokens). The server said: …".

**A reply that runs into the window** (a window stop: `model_context_window_exceeded`, or a llama.cpp `max_tokens` stop with more than 64 fewer tokens written than the budget) keeps today's SME-111/SME-126 path in `save_stop_notice` (`reply.rs`): the reply is kept and the notice saved — nothing is retried, a cut-off reply isn't continued. For the llama.cpp case the notice now says the limit was "the room left in the conversation" with the tokens written (`ReplyLimit::RoomLeft`'s wording; `save_stop_notice` picks it from the stop). When the call's whole measured size (input, cache, output) is below the turn's window by more than `COMPACTION_SAFETY_BUFFER`, that size is learned as the window and the next request compacts on its own.

## What smelt learns from the failure

The real window, saved for the model the turn ran on (`provider_id`, `model`), as the model's **Context window** setting — `provider_models.context_window`, the value `providers::context_window` (`providers/server.rs:198`) already prefers over `reported_context_window` and `context_window_for`, and the field the user can see and change in `providers/models.rs`.

- `db::lower_model_context_window(pool, provider_id, base_url, model, window) -> bool`, in `src/db/providers.rs`, is one statement: insert the row if missing; set `context_window` only where it's null or larger, and only while the provider's `base_url` is still the one the turn used. It only ever lowers, two learners settle on the smaller, and a provider moved to another server mid-turn isn't given the old server's window.
- `providers::learn_context_window(pool, &TurnModel, window)` (`providers/server.rs`) calls it and, on a change, publishes `ProvidersChanged` (`events::publish_app`). The providers page and pickers refresh; the meter takes the new window from the turn's next `ContextUsageUpdate` (published with the turn's copy, which the recovery updated first).
- **Ollama (Question 2: yes).** Ollama never refuses, so smelt looks for the signs after the fact. `message_delta`'s real input counts are already kept (SME-178 — done, not to redo). The new part: after an Ollama reply whose measured input is below half of smelt's estimate of the request (estimate at least 8,192), read `/api/ps` for the model's loaded `context_length` (`anthropic::models`, reusing `parse_ollama_ps` and `ollama_details`' fetch pattern); a smaller window is learned as above, so the next request in the turn, or the next turn, compacts first. The reply already written is kept (Trade-offs).

## What the user sees

- **Budget only:** the reply arrives after the refused attempt, no line in the transcript. The meter reflects the new window; the provider page shows it.
- **Compacted:** the usual "Conversation compacted" divider (`transcript.rs`), its header now saying why: "Conversation compacted: the model's server refused a request that didn't fit its 32,768-token window" (no window named: "…a request that didn't fit"). The reason is stored on the summary block — `ContentBlock::CompactionSummary` gains `refused: Option<String>` (serde `default` + `skip_serializing_if`, so existing rows and the replayed history are unchanged) — and `history_for_request` keeps replaying only the summary text, so the model never sees it (a test pins that).
- **Failed:** the worded reason goes where turn failures go today — for the user's turn, `note_failure` saves it as the conversation's failure notice and the turn ends `TurnFailure::Noted { reason }` (SME-123 DR3); for an agent turn, the `ServerFnError` text as before; reconnects keep `TURN_FAILED_NOTED`. Wording: "The conversation still didn't fit `flash-next`'s context window (32,768 tokens) after compacting. The server said: …" / "`flash-next`'s context window (4,096 tokens) is too small for smelt's system prompt and tools (about 9,000 tokens). Give the model a larger context on its server, or pick another model." / the compaction failure above.
- **Not an overflow:** unchanged.

No new controls: the existing divider (a longer header wraps in its `summary` row) and the existing Context window field.

## Spans (SME-137)

The `chat {model}` CLIENT span (`stream.rs`) records usage/finish reason and `mark_error`s on failure or cut-off; the `turn` span (`run.rs`) marks it on `TurnFailure::Failed`/`Noted`; `compaction` (`compaction.rs`) on a failed compaction. Written against them: the refused request really failed, so its `chat` span keeps being marked with the refusal (server message via `ModelError::Overflow`'s Display — the same failure-text sources that already reach `mark_error`; no new sink, per SME-137's rule). A **recovered** overflow must leave the **turn** span successful (recovery runs inside `run_turn_steps`; `run_turn_bounded` sees `Ok`), and a self-recovered compaction ends its span successful; only a second refusal marks turn or compaction failed. **Test (step 3):** turn span success, refused call's `chat` span failed naming the refusal, retry's `chat` span success.

## Slots (SME-343, SME-354 B6)

A turn takes its provider's slot before its first call and holds it to the end; its calls — `call_model` **and** `compact_conversation` — pass `SlotPolicy::Held` and never acquire (`docs/plans/sme-343.md`). The recovery's refused request, retry, and summary call send through the slot the turn already holds: no self-deadlock on a one-slot pool, and no other conversation slips between the refusal and the retry. `yield_to_own_agents` releases the slot only across a **tool pass**; the recovery lives inside one iteration before any tool runs. Cost: the turn's hold extends by the recovery's requests (§ Trade-offs).

## State model

**One model request** (keyed by the turn and its tool round), state × event:

| State | Overflow refusal | Other error | Reply streams | Stop |
| --- | --- | --- | --- | --- |
| **First attempt** | Learn W; then budget-only retry, compaction, or "can't help" (fail). | Fail, as today. | As today; a window stop learns its size. | Turn stopped; nothing saved for this request. |
| **Compacting** (first summary call) | Learn W; summarise once more with the smaller budget. | Fail, as today for a failed compaction. | Save the three compaction messages, then **Retry**. | Turn stopped; a partial three-message set that lands between inserts stays, as today. |
| **Compacting again** | Fail ("Couldn't compact…"). | Fail. | Save, then **Retry**. | As above. |
| **Retry** (after a budget cut or compaction) | Learn W; fail ("still didn't fit"). | Fail. | As today. | Turn stopped; the compaction and learned window stay. |
| **Done** | — | — | — | — |

A later tool round starts its own **First attempt** with its own one recovery; each recovery shrinks the request or fails, and `MAX_TURNS` bounds the turn. The Ollama thinking-corruption retries (`TOOL_CALL_PARSE_RETRIES`, inside `call_model`) stay inside each attempt, counted apart — one recovery wraps up to 1 + `TOOL_CALL_PARSE_RETRIES` sends, and an overflow refusal short-circuits that loop (the classifier runs first; `is_ollama_thinking_tool_call_corruption` can't match an overflow body).

**A retry racing a Stop:** the whole body runs under `run_turn_stoppable`'s `select!`, so a Stop drops it wherever it awaits — in the window write (one statement; committed or not, either is right); in the summary call (nothing saved); in the retry's stream (the partial reply is discarded, as for any stopped reply). `test_hooks` gains a pause before the summary call and before the retry, in its existing pause/receiver shape, for these tests. Nothing of the recovery is held in memory past the turn; a turn queued behind this one waits for the turn lock and sees the compacted conversation and the learned window.

**The window (`provider_models.context_window`). Writers:** (1) the user's model form (a form loaded before a learned write puts the old value back; last writer wins, the next refusal learns again); (2) a turn's refusal; (3) a compaction's refusal; (4) a window stop; (5) the Ollama `/api/ps` check; (6) provider deletion (cascade). Learners 2–5 only lower, in one statement, only while the provider's address is the turn's. Listings write `reported_context_window` (`db::set_provider_model_reported`), never `context_window` — a listing can't undo a learning. **Readers:** the turn that learned it (its updated copy, at once); another conversation's in-flight turn keeps the window it read at its start (SME-72's rule — on overflow it recovers on its own and its write changes nothing); `resolve_turn_model` for the next turn reads the new value; the meter (`conversation_context_window` on load, `ContextUsageUpdate` live); the provider page and picker re-list on `ProvidersChanged`. **Check then act:** between `should_compact` and the send another conversation may learn smaller — this turn may be refused, then recovers (Trade-offs, item 2). **Work in flight:** the recovery uses the turn's resolved `TurnModel`; a model picked mid-turn applies from the next turn, and the window is learned for the model the turn ran on. **Restart:** saved values stay, as today.

## Files (sizes at `59cd573`)

New (own estimates): `overflow.rs` ~10,000 with its inline tests; `turn/overflow.rs` ~8,000 (recovery decision, failure messages, window-stop check); `src/turn/tests/overflow.rs` ~13,000 — turn-level tests with a sequenced mock upstream (refusal → summary → reply), in the `run_turn_bounded`/`run_turn` fixtures `tests/turn_slot.rs` uses; one `mod overflow;` line in `src/turn/tests/mod.rs` (14,261).

Changed (now / est. after):

| File | now | est. after | change |
| --- | --- | --- | --- |
| `src/anthropic/mod.rs` | 1,703 | 1,750 | `pub mod overflow;`. |
| `src/anthropic/stream.rs` | 31,973 | ~33,500 | `ModelError`; classify at refusals and SSE `error` events; test wrapper maps `into()`. Over the aim — **no tests added to it**; new tests go to `src/anthropic/stream/tests/`. |
| `src/anthropic/types.rs` | 23,636 | ~24,200 | `CompactionSummary.refused`. |
| `src/anthropic/models.rs` | 33,350 | — | Over the aim with inline tests: **commit 0 is a move** — its `#[cfg(test)]` tests to `src/anthropic/models/tests.rs` (move only, every-target build). Then `ollama_loaded_window(endpoint, model)` reusing `parse_ollama_ps`, leaving ~21,000 in the module. |
| `src/turn/mod.rs` | 980 | 1,050 | `mod overflow;` + use line. |
| `src/turn/run.rs` | 35,541 | ~38,000 | recovery wired at `call_model` in `run_turn_steps`; window-stop learning call. Bodies live in `turn/overflow.rs`; keep additions under ~2.5 KB — 4,459 under the ceiling; if review needs more, split its own sections first (rules-commits.md). |
| `src/turn/call.rs` | 16,807 | ~17,300 | `call_model` returns the typed error (overflow refuses short-circuit the parse-retry loop); corruption check reads the message. |
| `src/turn/compaction.rs` | 22,294 | ~24,300 | learn, recover own overflow once, typed failure; pass `refused` into `compaction_messages`. |
| `src/turn/test_hooks.rs` | 7,342 | ~8,300 | pause before the summary call and before the retry (Stop-race tests), in the file's existing pause/receiver shape. |
| `src/providers/server.rs` | 36,042 | ~36,800 | `learn_context_window` next to `context_window()`. 3,958 under the ceiling, not within 2,000; tests to the existing `src/providers/tests/`. |
| `src/db/providers.rs` | 16,552 | ~17,800 | `lower_model_context_window`; tests in `src/db/tests/`. |
| `src/frontend/pages/chat/transcript.rs` | 24,496 | ~25,000 | the divider header's reason (from `refused`). |
| `src/frontend/pages/chat/tests/reply_parts.rs` | 11,760 | ~12,600 | divider wording tests (SME-105's reply-part tests live here; `tests/notices.rs` is over aim — not adding there). |
| `src/browser_tests/overflow.rs` + one `entry` line in `mod.rs` | new / 22,260 | — | scenario `overflow_compacts` (new scenarios don't count for overlap). |
| `docs/api.md` | 27,859 | ~28,600 | a paragraph after the cut-off material: refusals that mean "didn't fit", the recovery, what's learned. |

`src/turn/history.rs` (24,554): **no change** — `history_for_request` already replays only the summary text; a test asserts a `refused` reason never reaches the model. At close-out (rules.md): the Current state document and a Feature checklist row (`docs/feature-checklist/models.md`). No migration (`context_window` exists), no `Cargo.toml`, no image rename, no env var (checked against the dev `.env`).

## Steps (one commit each, test-first; red predictions marked, recorded by the Developer)

0. **Move only:** `models.rs` tests → `src/anthropic/models/tests.rs`; green on a build of every target.
1. **`overflow::classify` + `ModelError`.** Every cited body a test, the must-not-count set beside it; `stream_anthropic_message_metered` returns `ModelError` (Display = today's text), wired at both production callers and the test wrapper in this commit (no dead-code gate break). Expected first red: the hermes-agent mistake — the window parsed from "A + M > W"'s first number — before the shapes rule.
2. **Learning:** `db::lower_model_context_window`, `providers::learn_context_window`, `ProvidersChanged`. Tests: lowering only; two learners settle on the smaller; provider moved mid-turn (base_url guard); the user's form save after a learned write; listings never touch `context_window`.
3. **The turn's recovery:** budget-only retry, compact-then-retry, can't help, refused twice — each test asserts what's saved (learned window, divider rows, kept reply), what's published (`ContextUsageUpdate` with the new window), the final transcript, and that **spans read as intended** (§ Spans). Stop-race tests use the two new hooks. Parts check (tdd.md): the budget-only test red with only the compaction branch, and vice versa.
4. **A compaction that overflows:** one retry at `compaction_transcript_budget(W)`, the "Couldn't compact…" wording, typed failure out of `compact_conversation`; its span green after recovery, red after the second refusal.
5. **Window stops:** llama.cpp detection (>64 tokens short of budget with a full call), size learning past `COMPACTION_SAFETY_BUFFER`, the `RoomLeft`-worded notice on that path; existing cut-off tests unchanged unless the stop reason forces wording.
6. **Ollama's quiet truncation:** the post-reply estimate-vs-measured check and `ollama_loaded_window`; one test asserts the Ollama `message_delta` shape fixes the meter (SME-178's tests carry the rest).
7. **What the user sees:** `CompactionSummary.refused` (serde round-trip, old rows, history replay never sends it), the divider header, `docs/api.md`.
8. **Browser scenario `overflow_compacts`:** mock's first answer is llama.cpp's 400 body; asserts no error line, the divider reads with the window, the reply lands; `compaction_divider` stays green.

`TurnFailure`/`note_failure` interplay is exercised by step 3's failure tests (user turn → notice; agent turn → error text).

## How it will be checked by hand

The mock scenario is the automated proof (a real refusal needs a prompt over a real model's `n_ctx`; the user's one-slot llama-server must not be driven from a seat). Hands-on, post-merge, the user's machine: set a provider's **Context window** deliberately too big for its server, send until the proactive check can't catch it — watch the divider's reason and the provider page's field; then set the field below the system prompt's size and confirm the "too small" notice.

## Overlap check (2026-11-03, board today, main `59cd573`)

Every non-Done/non-Canceled ticket whose plan or branch touches a file above:

| Ticket | Status | Files (from its plan or branch) | Overlap |
| --- | --- | --- | --- |
| SME-96 (check-server previews) | **In Progress** | `scripts/check-server`, `scripts/test-check-scripts`, `docs/testing.md`, `docs/setup.md` — "No Rust" per its plan | None. |
| SME-71 (desktop notifications) | Up Next — plan of record on branch `sme-71-desktop-notifications` (`docs/plans/sme-71.md`) | move commits split `src/turn/run.rs` (its lines 51–360 → `lifecycle.rs`); `turn/state.rs`, `turn/tests/mod.rs`, `browser_tests/mod.rs`, events, frontend | `run.rs`, `turn/tests/mod.rs`, `browser_tests/mod.rs`. Not a blocker under the rule (it stops on Planned/Todo/In Progress; SME-71 is Up Next; the `mod`/`entry` lines are appends; new scenarios don't count). Its run.rs move and this plan's `run_turn_steps` edits are disjoint hunks; second merger rebases. Its plan's run.rs figure (39,589) is stale (35,541). |
| Turn-slot follow-ups SME-346/347/348/349/358; ideas SME-200 (`providers/server.rs`), SME-201 (`stream/lines.rs`), SME-55, SME-188, SME-116, SME-290, SME-357, the thinking trio SME-74/98/120 | **Backlog** ideas — no plan section, no GitHub branch | file-adjacent ideas only | None — the rule counts a ticket's plan or branch, and these have neither. SME-55 ("typed statuses and errors") is conceptually upstream of this plan's `ModelError` and stays Backlog. |
| `pm-plan-after-dependencies`, `cconstan/init-container-memory` | branches, no blocking ticket | `docs/development-process.md`; `terraform/smelt/*` | None. |

All of the 2026-10-09 blockers are Done (SME-137; SME-100/119/105/122/123/144).

## Trade-offs to flag

- **Ollama's kept cut-off reply:** Ollama truncates quietly and answers anyway. Smelt keeps that reply — it only learns the window (`message_delta` usage + `/api/ps`) so the *next* request compacts first. Re-running the truncated reply is not attempted.
- **Check-then-act costs one refused request:** a window learned by another conversation between this turn's `should_compact` and its send is met by one refusal + recovery. No data is lost.
- **Gateways that hide the real error** (claude-code-router's "All target providers failed.") stay unrecognised: the turn fails as today. Nothing can classify what never arrives.
- **A second refusal fails the turn.** One recovery per request bounds latency and cost; the next turn starts with the learned window.
- **Recovery extends the turn's slot hold** (SME-343): on a one-slot server another conversation can wait through the refused request, the summary call and the retry — up to two extra requests per refusal (three if the summary itself overflowed and retried once). Same kind as SME-343's Trade-off 1; fairness yielding stays SME-347's.
- **Learners only lower:** if the server's window later grows, smelt keeps the smaller learned value until the user raises the Context window field (a refusal can't raise it). Visible where it's set; stated in `docs/api.md`.

## Questions for the user

None. The 2026-10-08 questions were Decided 2026-11-03 (kept as the Context window setting; Ollama handled here) and are folded above.
