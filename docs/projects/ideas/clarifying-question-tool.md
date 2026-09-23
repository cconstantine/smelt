# An explicit ask-the-user tool (question)

## What

A tool the model calls to explicitly pause and ask the user something —
a real structured prompt (optionally with choices), distinct from just
writing a question as its normal reply text. Same idea as opencode's
`question` tool (and this harness's own `AskUserQuestion`).

## Why

Today the model has exactly one way to communicate with the user: its
regular streamed reply. A clarifying question ("which of these two
approaches?", "should I delete this file, it looks important") currently
looks identical, structurally, to any other assistant message — the UI has
no way to render it distinctly (e.g. as a blocking prompt with real
buttons for the offered choices) or to treat the turn as genuinely paused
on a human answer rather than just having said its piece and stopped. For
a coding agent making real destructive-capable changes inside its sandbox
(`coding-session.md`'s "Open questions" already flags "confirmation/
approval UX for destructive tool calls" as unresolved), a structured way
to ask before acting is the more direct fix than inferring intent from
plain text.

## Depends on

- A new `ContentBlock` variant (or a specially-shaped `tool_use`/
  `tool_result` pair) the frontend can recognize and render as an actual
  interactive prompt — buttons for offered options, a text box otherwise —
  rather than plain markdown text, the same way `frontend/pages/chat.rs`
  already special-cases rendering for `ToolUse`/`ToolResult`/`Thinking`
  blocks today.
- The turn loop (`run_turn_bounded` in `src/api/chat.rs`) needs a real
  "paused, waiting on the user" state distinct from "waiting on the next
  Anthropic API response" — the bounded `MAX_TURNS` loop currently assumes
  every `tool_result` comes back promptly from tool execution, not from an
  arbitrarily-delayed human. Likely closer in shape to how a background
  task's completion notification wakes a stalled conversation
  (`wake_conversation`) than to an ordinary in-turn tool call.
- Persistence: the question and its eventual answer both need to survive
  a reload, same as everything else in `Message.content`.

## Open questions

- Does the model get to offer multiple-choice options (structured, like
  this harness's own `AskUserQuestion`) or is it always free-text?
- What happens if the user never answers — does the conversation just sit
  paused indefinitely, or is there a timeout/reminder, similar in spirit to
  a stalled background task?
- Overlap with the todo-list idea (`docs/projects/ideas/todo-list-tool.md`):
  both are about the model's own process becoming more visible/
  interactive rather than purely generative — worth checking they don't
  end up duplicating UI machinery if both get built.
