# Pod management

## What

Let the user see and manage sandbox pods, not just the model.

- **See at a glance which conversations have a live pod.** A marker next to each conversation in the sidebar, updated live, so the user can scan the list without opening each one.
- **A pods view.** One place listing every live pod across all conversations: its conversation, how long it has been up, its status (`Running`, `Pending`, ...), its memory/CPU limits and how many terminals are open.
- **Basic actions.** Stop a pod, and restart it (stop, then create a fresh one with the same limits). Both are available from the pods view and from the conversation's own sandbox panel.

## Why

Today only the model creates and ends pods, through `create_pod`/`terminate_pod`. The sandbox panel shows a pod only once the user has opened that conversation, and the sidebar says nothing. A pod runs until the model terminates it or the conversation is deleted, so pods left behind in old conversations keep holding cluster memory and CPU with no sign that they exist. The user's only way to find them is `kubectl` against `smelt-park`.

## Depends on

- **A cross-conversation signal.** `events.rs` is a per-conversation bus, and `ChatPanel` subscribes only to the open conversation. A live sidebar marker needs either a global event stream or pod status included in `get_conversations`, refreshed on some trigger.
- **A cross-conversation query.** `db::list_sandbox_pods` and `sandbox::list_pods` are scoped to one conversation. The pods view needs all live rows (`terminated_at IS NULL`, already indexed) joined with conversation titles, plus each pod's live phase from Kubernetes.
- **Stopping a pod that has terminals.** `terminate_pod` refuses while a terminal is live (`TerminalStillExists`). A user stop should tear the terminals down first, or go through `force_terminate_pod`, marking any running commands as lost the same way a crash does.
- **Telling the model.** If the user stops or restarts a pod mid-conversation, the model's next turn must learn about it, or it will call tools against a pod that is gone. The pod-crash path from `20260816-sandbox-oom.md` already reports a pod's death to the model in one message; a user-initiated stop could reuse it with a different reason.

## Open questions

- **What does restart mean?** Pods are disposable: a restart loses the terminals, their shell state, and anything written outside a mounted volume. Is that still useful as a "fix a stuck pod" button, or should restart keep something (the terminals it had, re-created empty)?
- **What if the model is mid-turn?** Should stop/restart be refused, or should it go ahead, cancel in-flight commands, and let the model see the failures?
- **Should the user be able to create a pod?** Starting one the model then picks up would make the pods view complete, but it is not needed to solve the visibility problem.
- **Idle cleanup.** `coding-session.md`'s "Deleting idle pods" item is the automatic version of the same problem. It could land here (show idle time in the pods view, then an optional timeout) or stay separate.
- **Resource usage.** Showing live memory/CPU use per pod needs `metrics-server`, which may not be installed on `homelab`. The configured limits are available either way.
