# A top-level conversation set to a persona (SME-481)

Part of [the personas design](index.md).

Until SME-481 a persona was only ever a child's: `start_agent` read the persona file in the child's checkout and snapshotted it into the child's `agents` row, and every reader of a persona read that row. A top-level conversation has no `agents` row, so it ran allow-all, held the git key whenever one was stored, held no named secret and had no `# Persona` section. A Ticket Driver or Project Manager started by the user, or by a PM through the API (SME-460), got its persona only from prose in its brief.

**What changes:** a top-level conversation can be **set to a persona once, after its repo is attached and before its first message**. From then on it runs under that persona's instructions, tool rules (`own` included), credentials and `smelt.model`, as a child with that persona does. Why now: a PM-started Driver must be top-level (one tree per ticket, SME-460), and its rules, credentials and model were prose until this.

**What still holds:**
- **The trust gate at the read.** The persona is read from the conversation's own checkout, and only from a remote the user trusts. Its options are listed only for a trusted remote, and the set reads the trust again under the remote's trust lock (`db::lock_remote_trust`) in its transaction. Nothing from an untrusted file (a description, a setting or a parse error) reaches the listing or a refusal.
- **The snapshot is frozen.** A later edit of the file changes nothing until a [refresh](#refresh), which reads only what reached the remote's default branch, so a persona can't widen itself.
- **Credentials only from what the persona lists.** Rule G (`git-ssh`) is granted only if the persona lists it, and Rule S's named secrets come from its own snapshot alone ([tool-rules.md](tool-rules.md#credentials)). A top-level persona gets exactly what a child of the user's persona-less root with the same persona gets, and nothing more.
- **`own` binds only its own conversation.** Its agents get its chain through `for_child`, which strips `own`.
- **Never set or swapped once started, only refreshed.** There is no unset and no swap: a set on an applied persona is a [refresh](#refresh) of the same persona.
- **Its model resolves only on its own provider** (below). That is narrower than a child's resolution, never wider.

## How it's set

`POST /api/conversations/{id}/persona` with `{"persona": "<dir>:<name>"}`, scoped names only. `GET` on the same path lists the options or says what it's set to. The UI's picker on a new conversation's start screen, under "Work on a repo", uses the same two calls. `POST /api/conversations` is unchanged: the persona is read from the conversation's own checkout, which exists only once the attach's asynchronous clone finishes. A set on a `ready` checkout is synchronous. The PM's sequence is create, attach, wait until the persona is listed, set the persona, set the model, send ([pm-in-smelt.md, "Starting a Driver"](../../personas/pm-in-smelt.md#starting-a-driver), which gives the reasons). With the persona set first, its `smelt.model` resolves on the default model's provider ([below](#model)).

## "Started", and where it's enforced

A conversation has **started** once it has any `messages` row, a turn holds or is queued on its turn lock, or a turn has been admitted (`turn::turn_running`, which counts a turn from `TurnInFlight::start_by_user` before it takes the lock). `agents::top_level::set_persona` runs the whole set inside `turn::between_turns(id, wait = false, …)`:

- A lock held or queued for, or `turn_running`, refuses: "this conversation has started".
- In one transaction: the remote's trust lock first (the lock order `create_child` uses), then the conversation's row `FOR NO KEY UPDATE`. It refuses a conversation that is gone, has a parent ("an agent's persona is set by start_agent"), is being deleted, has any message, or already has an applied persona. Otherwise it inserts the row unapplied (or replaces an unapplied one), and pins the model.
- The apply finishes before the turn lock is released: the pod reconciled to the persona's grant (`credentials::reconcile_pod`, the child's call), the checkout's AGENTS.md loaded if the conversation doesn't have it yet, and the row marked applied. A send admitted during the set waits for the lock and runs under the persona.
- **The admission gate** covers the window a restart can leave. `admit_user_turn`, `confirm_user_turn` and `admits_notices` read one query, `db::admission_state`: the agent's state, or `persona-pending` for an unapplied row. They refuse such a turn ("this conversation's persona is still being applied"). The startup sweep puts every unapplied set back: it runs while smelt already serves, so it waits for each conversation's turn lock (a set started since the restart finishes and is applied, not swept). A failed set, and the sweep, put back the model and pod resources the set changed, delete any trust card its AGENTS.md load filed, and write the persona-less grant into the pod; only then is the row deleted, under the same install lock. A put-back that fails keeps the row, so no turn runs on a pod that may still hold the persona's grant.

## The table, `top_level_personas`

One row per conversation set to a persona: the snapshot (`persona_name`, `persona_path`, `persona_commit`, `persona_body`, `persona_settings`, as a child's `agents` columns), `rule_chain` (the persona's one `smelt.tools` entry, `own` included, or NULL), `credentials` (Rule G's bit: `{git-ssh}` or `{}`), `created_at` and `applied_at`. It is a table of its own rather than an `agents` row at depth 0 because about 47 queries read `agents` as "this conversation is a child". A root row there would need every one audited, and one missed would be a parent notice to nobody or a `finish` offered to the user.

**State:** none → unapplied (inserted) → applied (`applied_at` set) → applied (refreshed: the snapshot columns rewritten), then gone with the conversation's cascade delete. **Writers:** the set (insert, replace-unapplied, mark applied, and the compensating delete of a failed apply), the refresh (only `WHERE applied_at IS NOT NULL`; the set's insert only where it's NULL), the startup sweep (delete unapplied) and the cascade. **Readers**, beside `agents`:

| Reader | What it reads |
| -- | -- |
| `db::agent_rule_chain` (the offer, the native and MCP calls, the question tools, script MCP calls) | the chain |
| `db::agent_credentials` (Rule G) and `db::persona_credential_names` (Rule S) | the bit and the snapshot's names |
| `db::agent_persona` (the `# Persona` section, "read when this conversation was set to it or last refreshed") | the snapshot |
| `db::conversation_tree` (the sidebar's persona line, with no status) | the name and path |
| `db::create_child` (a top-level caller's children) | the chain through `for_child`, and the bit as the child's seed |
| `db::admission_state` (the gate) | whether it's still unapplied |

SME-484's `smelt.resources` rides the same snapshot: the set makes the conversation's `pod_resources` the persona's and restarts its pod with them (`persona::match_pod_resources`, a child's own step) before the credentials are reconciled. A later snapshot field reaches a top-level persona through the same `persona_settings` a child's does.

## Refresh

SME-516. A set on a conversation whose persona is applied is a **refresh**: `POST …/persona` with `{"persona": "<dir>:<name>", "commit": "<sha>"?}` (`agents::top_level_refresh`). It re-reads **the same persona** (another persona's name is refused: a swap is a new conversation) and replaces the snapshot: the body, `persona_settings` (`compact_at`, `tools`, `credentials`, `resources`, `ignored`), `rule_chain`, Rule G's bit and `persona_commit`. `prior_*`, `created_at` and `applied_at` stay. Each setting takes effect from the conversation's next turn: a turn reads its ceiling, chain and prompt at its start. The model isn't re-pinned (the conversation keeps its own), and children already started keep their chain; children started later get the new one through `for_child`. A `commit` on a first set is refused: a first set reads the checkout as it is.

**Its input is only the remote's default branch.** A live seat can edit, commit and push its own persona file, and a top-level chain has no ancestor to intersect with. So a refresh reads the file only at a commit that is an ancestor of `origin/HEAD`, after a fetch (`git::persona_at_default_branch`, `git show <sha>:<file>`; a symlink or an oversized file is refused). With no `commit`, it reads `origin/HEAD`'s tip. It never reads the working tree or another branch. What reaches `main` has been through review. The trust gate holds as the set's does: read before the read, and again under `lock_remote_trust` in the write transaction.

**It applies under the turn lock.** The read runs outside it. Then `turn::between_turns_or_queue`: with the lock free it applies at once (`applied`). Otherwise the apply takes its place in the lock's FIFO queue and runs in a task of its own once the turn ends, before any turn queued after the request, and the answer is `pending`. A seat refreshing itself is always `pending`. One refresh at a time per conversation: an in-memory map holds each conversation's last refresh (`pending`, `applied` or `refused` with why), which the `GET` returns as `refresh` beside the snapshot's `settings`. A second request while one is pending is refused. A restart drops a pending refresh with nothing written.

**The apply:** the row is read again under the lock (still applied, same path). If the snapshot's `resources` differ from the conversation's `pod_resources` on a live pod, it's refused while any command runs there (a restart would end them); otherwise `pod_resources` is written and the pod restarted. Then `credentials::refresh_grant`, under the install lock, writes the pod's files for the new grant and only then the row, in one transaction that reads the trust again. A failed transaction puts the pod back to the row's grant (`reconcile_pod`).

## Model

A top-level persona's `smelt.model` resolves **only on the provider the conversation is on at the set**: its chosen provider (`conversations.provider_id`, set by the picker or `POST …/model`), else the default model's provider. It is matched in `provider_models`.
- If that provider lists the model, the conversation is pinned to it, and open tabs are told (`ModelChanged`).
- If it doesn't, the set is **refused, naming the providers that list it**, and nothing is written. There is no fallback to the default's provider or to others by name. That fallback is how a ClaudeCode user would land on the paid API (SME-460).
- With no model at all, the set is refused: "pick a model first".
- With no `smelt.model`, the conversation's model is left alone.

After the set, the picker still works: the model isn't a security boundary. A child's resolution is unchanged ([persona-files.md](persona-files.md)); SME-485 is its follow-up.

## Trade-offs

- **The trust gate binds files, not API callers.** `POST /repos` trusts what it attaches, and the API is unauthenticated, so a caller that can reach smelt can attach and set a persona. That adds no reach `start_agent` didn't already have; SME-460's client and SME-243's egress rules are the guard.
- **A failed attach that saved a notice counts as started.** Start a new conversation.
- **`admits_notices` falls open on a database read error,** as it did before; with an unapplied row the window is only between a restart and the sweep.
