# A child's checkout, and volumes
Part of the [personas design](index.md) (SME-234).

Decided by the user (2026-10-18): `start_agent(brief, repo?, persona?, volumes?)`, none of them required. With `repo` the child's sandbox starts with that repo cloned and its AGENTS.md loaded before its first turn; `persona` requires `repo` and is loaded before the first turn. There is no automatic shared volume: the parent creates volumes and passes them; a volume the model created is deleted with the conversation that created it; a parent passes only volumes it created or has mounted. This page decides the rest.

## Which ref the child's clone is on

A child has its own pod and `/workspace` claim, like every conversation, and its repo is a fresh clone **from the remote**, so the parent's unpushed work isn't in it.

**Decision: a `ref` argument, plus volumes for anything not pushed.**

- `start_agent`'s `repo` is one of the parent's checkouts, by its path (`/workspace/smelt`), and must be `ready` (`conversation_repos.status`). The child clones the same URL into the same directory name.
- `ref` (optional) is a branch, a tag or a commit on the remote. Without it, the child gets **the branch the parent's checkout is on now**, read from the parent's pod (`git symbolic-ref --short HEAD`), not the branch recorded at the parent's clone, which may be stale. A parent on a detached `HEAD`, on a branch the remote doesn't have, or on one whose `HEAD` isn't the remote branch's tip (compared with `git ls-remote`, so commits not yet pushed aren't silently left out) is refused with what to do: push it, pass `ref`, or share the work on a volume.
- A commit can't go through `git clone --branch` (`clone_command`, `src/git/server/clone.rs`): SME-236 clones the default branch and then fetches and checks out the commit.
- **Unpushed work goes on a volume** (SME-239): the parent creates one, puts a clone or a `git worktree` of its work on it, and passes it; the child works there. The child's own `/workspace/<dir>` clone still exists, and is where its persona and AGENTS.md are read from.

**Alternatives:**

| Option | Against |
| -- | -- |
| Always the remote's default branch | A Code Reviewer for a PR needs the PR's branch |
| The parent's working tree copied into the child (a `git bundle` from the parent's pod, through smelt, unbundled in the child's) | Carries commits but not uncommitted changes, streams a whole repo through smelt, and is a second clone path to keep; a volume already carries everything |
| Only volumes, no clone | The persona and AGENTS.md must come from a checkout smelt controls the ref of, read before the first turn |

**Why:** the process already pushes before it hands work on (the Developer pushes before its Code Reviewer starts), so a ref covers the usual case with today's clone code; a volume covers the rest without a second mechanism.

## The parent's trust carries to the child

Trust is per remote, not per conversation: `repo_trust` is keyed by `remote_key` and belongs to no conversation (`docs/database.md`, "Git"). So a child cloning a remote the user trusts is trusted already, and **no trust card is shown**.

`start_agent` with `repo` requires `repo_trust.trusted = true` for that checkout's `remote_key`, read under `db::lock_remote_trust` in the same transaction that records the child's `conversation_repos` row (as `cloning`), as `request_or_load` does (SME-181). A remote the user hasn't decided about, or declined, is refused: "the user hasn't trusted this repo; load its AGENTS.md with `load_instructions` so they're asked". The child can only name a repo its parent has checked out, so it can't reach a remote the parent couldn't.

Before its first turn, smelt loads the child's top-level `AGENTS.md`, if the checkout has one, through `request_or_load`, which finds the remote trusted and loads it. Should the user Forget the remote on the Git page between the start and the clone, the persona read fails, so the start fails: the child ends `failed`, the card its AGENTS.md load made is deleted, and the parent is told ([agents.md](agents.md#how-a-childs-report-and-messages-reach-a-conversation)). The child's first turn waits for its clone (not `run_turn_steps`'s 120-second `CLONE_WAIT`): the persona has to be read first.

## Volumes

### The volume tools

- **`create_volume(name, mount_path)`**: a claim owned by this conversation, mounted at `mount_path` in its own pod. `name` is 1–64 letters, digits, `-` and `_` (global volumes' names have no rule; a model-written one gets one), unique per owner. `mount_path` validated like a global volume's (`validate_mount_path`, `~` expanded), and refused at or under `/workspace`, at or under `/etc/smelt` (`POD_GIT_DIR`: the key would be written onto the volume and passed on with it), at or under a system directory (`/etc`, `/usr`, `/bin`, `/lib`, `/proc`, `/sys`, `/dev`, `/var/lib/docker`), at a global volume's path, or at another of this conversation's mounts. Credential narrowing can't stop a parent copying a file it can read onto a volume it passes; a parent without `git-ssh` has no key to copy.
- **`delete_volume(name)`**: one this conversation created. Refused while a `starting` or `active` agent other than this conversation mounts it (stop or finish it first).
- The model sees its mounted volumes in the system prompt's Environment section, as global volumes are listed today (`turn::system_prompt`), each with whether it owns it.

**A pod's volumes are fixed when it starts.** Kubernetes can't add a volume to a running pod, so a volume created while the conversation has a live pod mounts from its next start, and the tool's result says so; the model restarts its pod (`terminate_pod`, then `create_pod`; `/workspace` is a claim and survives) when it's ready to. smelt doesn't restart it on its own: that would kill the model's running commands.

### The schema

- **`agent_volumes`**: `id`, `owner_conversation_id` (FK → `conversations` cascade), `name`, `mount_path`, `created_at`; `UNIQUE (owner_conversation_id, name)`.
- **`conversation_volume_mounts`**: `conversation_id` (FK cascade), `volume_id` (FK → `agent_volumes` cascade), primary key both. The owner has a row too.
- The claim is `agent-volume-<id>`, `ReadWriteOnce`, `SANDBOX_VOLUME_STORAGE_SIZE`, labelled `smelt/instance=<instance id>` and `smelt/agent-volume-owner=<conversation id>`, built like `build_volume_pvc_spec` (`src/sandbox/spec.rs`). Created, mounted, swept and deleted only by its own instance (`counts_as_ours`, SME-115).
- A pod's spec mounts the global volumes and its conversation's `conversation_volume_mounts`.

The global `sandbox_volumes` table isn't reused: those are the user's, mounted in every pod and never deleted with a conversation.

### Passing a volume to a child

`start_agent(volumes: [name…])` mounts each in the child's pod at the volume's own `mount_path`. Each must be one the parent has a `conversation_volume_mounts` row for (it created it, or it was passed one). Checked and recorded in `start_agent`'s transaction, which locks the volume rows (`FOR SHARE`) against a concurrent `delete_volume`. A child never gets a volume its parent doesn't have.

### Deleted with the conversation that created it

A volume is passed only down the tree, so every conversation that mounts it is the owner or one of its descendants. Deleting the owner deletes its whole subtree ([agents.md](agents.md#deleting)), so nothing mounts the volume afterwards. The delete lists the subtree's volumes before the rows cascade, deletes their claims after the pods are torn down (Kubernetes' `kubernetes.io/pvc-protection` holds a claim until no pod uses it), and a sweep beside `sweep_orphaned_conversation_claims` deletes any claim of ours whose owner is gone, for a delete cut off part-way.

### Sharing a claim between pods

Claims are `ReadWriteOnce` (`src/sandbox/spec.rs`). **RWO means one node, not one pod** (one pod is `ReadWriteOncePod`). Spike S2, on a throwaway k3s `v1.34.6-k3s1` in this host's Docker: one `local-path` RWO claim, mounted by two pods at once, each appending 20 lines; both pods ran on the one node, and each read the other's 20 lines.

smelt requires a single-node k3s with `local-path` (`docs/setup.md`, "A single-node k3s cluster"), and global volumes already rely on this: every pod mounts every global volume at once. **Decision: RWO, no pinning code.** On a cluster of several nodes, a `local-path` volume carries the node it was made on as node affinity, so the scheduler puts every pod that mounts it there (pinning, at that node's capacity); a network storage class with RWO would fail to attach a second pod on another node, and would need an RWX class. That limit is written where the volume tools are documented, as it is for global volumes.

### Concurrent writers

What smelt tells the model (the tool's description) and the persona docs say:

- **One working tree per agent.** A shared clone on the volume, and `git worktree add <mount>/<agent>` for each agent, on its own branch. Two agents never work in one tree. git locks the index and each ref, so separate worktrees of one repo can work at once; run `git config gc.auto 0` on the shared clone, so one agent's automatic `gc` doesn't repack under another's fetch.
- **Builds:** a `CARGO_TARGET_DIR` per agent unless they build the same tree one after the other. cargo locks a target directory ("Blocking waiting for file lock on build directory"), so a shared one is safe but serializes builds. A shared `CARGO_HOME` is fine: cargo locks its package cache. Likewise a `node_modules` per working tree.
- Docker images aren't shared: each pod's Docker has its own data claim.
