# A model-visible todo list tool (todowrite/todoread)

## What

A small pair of tools — write/replace a structured task list, read it back
— that let the model track its own multi-step plan explicitly, rendered
live in the UI the same way the background-task and sandbox panels already
are. Same idea as opencode's `todowrite`/`todoread`.

## Why

Long tool-use turns (a multi-file refactor, a multi-command debugging
session) currently leave the model's plan implicit — it's wherever the
model last said it in plain text, with no structured way for either the
model or the user to check progress against it partway through a long
turn. A todo list makes that plan a first-class, inspectable piece of
state instead: the model updates it as it works, and the user watching the
transcript sees the same progress the model is tracking internally, the
same "don't just receive a final message, watch it happen" principle
`coding-session.md`'s "Visibility" section already establishes for the
sandbox panel.

## Depends on

- No sandbox/pod involvement at all — unlike every other tool idea here,
  this is pure conversation state, not something that touches the model's
  coding environment. It likely doesn't need `sandbox.rs`/
  `sandbox_agent.rs` in the loop at all.
- A new small table (e.g. `conversation_todos`, keyed by `conversation_id`)
  alongside the existing `db.rs` CRUD pattern — a todo list is
  conversation-scoped and needs to survive a reload, the same reasoning
  `models.md` gives for storing `Message.content` as structured
  `ContentBlock`s rather than throwaway state.
- A live panel, following the same shape as the background-task and
  sandbox panels already in `frontend/pages/chat.rs`: a `ConversationEvent`
  variant (alongside `TaskUpdate`/`SandboxPodUpdate`/etc. in
  `src/events.rs`) so an update streams to the browser the instant the
  model calls the tool, not just on the next full snapshot.

## Open questions

- Is the list a single flat replace-the-whole-thing call each time (like
  opencode's `todowrite`, simplest to reason about — no partial-update
  races) or does it support per-item updates?
- Does the model see its own prior todo list on the next turn
  automatically (part of context, like a `system`-prompt-adjacent
  reminder), or does it have to call `todoread` explicitly each time?
  Affects whether this actually reduces the model losing track of a plan,
  or just adds a tool it has to remember to call.
