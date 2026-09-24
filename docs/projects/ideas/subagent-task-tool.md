# A real subagent dispatch tool (task)

## What

A `task` tool that spawns a genuinely separate model context — its own
system prompt, its own conversation, working a sub-problem independently —
and reports a result back to the calling turn, the way opencode's `task`
tool (and this harness's own `Agent` tool) does. Distinct from anything
smelt has today.

## Why

`run_async` (see `state.md`) backgrounds a single *tool call* — `fork`+
`exec` for one invocation, checked on later with `task_status`/
`task_result`. A `task`-style subagent is a different thing entirely: a
whole separate model, with its own reasoning and its own sequence of tool
calls, delegated a piece of work and trusted to figure out how to do it.
The gap this closes: today every tool call, however small, happens in the
one conversation's own turn loop and context — there's no way to hand off
"go investigate X and tell me what you find" without it consuming the
parent conversation's own context budget for every intermediate step.

## Depends on

- This needs its own conversation row (or an unlisted/child variant of
  one) so the subagent's turns persist through the same `run_turn`
  machinery `src/api/chat.rs` already has, rather than inventing a
  parallel execution path. Likely the single biggest design question here
  — everything else in this idea list slots into the *existing* tool
  dispatch table in `src/anthropic/tools.rs`; this one asks whether
  `run_turn`/`Conversation` can host a "child" run at all.
- The result needs to come back to the parent conversation as a single
  `tool_result` — the subagent's own back-and-forth (its deltas, its tool
  calls) shouldn't stream into the parent conversation's transcript the
  way `subscribe_conversation_events` does today, or the parent's
  transcript becomes unreadable. Does the subagent get its own,
  separately-viewable transcript (a real child conversation, visible in
  the sidebar) or is it fully hidden, only its final answer surfacing?
- Sandbox scope: does a subagent get its own sandbox pod, or share the
  parent conversation's one live pod (`create_pod`'s "at most one per
  conversation" rule doesn't obviously answer this for a child
  conversation)?

## Open questions

- Recursion: can a subagent itself call `task` to spawn a further
  subagent, and if so what bounds that (a depth limit, alongside
  `MAX_TURNS`'s existing per-turn bound)?
- Cost/latency: a subagent call is a full nested `run_turn` loop, plausibly
  many Anthropic API round trips, before the parent tool call even
  resolves — worth understanding the practical cost profile before
  offering this to the model freely.
- This is architecturally the largest idea on this list — probably needs
  its own plan rather than being scoped as a small tool addition like
  `glob`/`grep`.
