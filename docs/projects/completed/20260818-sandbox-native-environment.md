# Sandbox pods that behave like a real dev machine

**Branch:** `sandbox-native-environment` · **Idea:** `projects/ideas/sandbox-native-environment.md` (removed) · **Plan:** `projects/plans/sandbox-native-environment.md` (removed)

## What shipped

Four related changes to the sandbox pod, agreed on and refined across an
extended planning conversation before any code was written:

- **The sandbox agent is now the pod's real `ENTRYPOINT` (genuine PID 1)** —
  `src/sandbox.rs`'s old `inject_and_launch` (tar-over-exec injection of a
  binary `include_bytes!`'d into `smelt` itself, then a `setsid`-detached
  launch) is gone entirely, along with the `AGENT_BINARY` static.
  `ensure_pod_connection` collapsed from "connect, and if that fails inject
  and launch" to just "connect, with a short bounded retry." Spiked
  end-to-end against the real hermetic test cluster with the actual
  unmodified `sandbox_agent` binary as PID 1 before trusting it: created a
  terminal, ran `sleep 300`, sent `signal: INT`, got `exit code 130` back
  in under a second — the existing SIGINT-reset-to-default logic needed no
  changes.
- **Commands run as a regular, pre-created `sandbox` user by default, with
  passwordless `sudo`** — entirely a `docker/sandbox/Dockerfile` concern
  (`useradd -m sandbox`, a NOPASSWD sudoers entry, `USER sandbox`); no
  `sandbox.rs`/`sandbox_agent.rs` code changes needed at all.
- **A custom sandbox image, delivered with no registry** — smelt had no
  image registry or publish pipeline before this (it only ever ran via `dx
  serve`/`cargo run`). Rather than standing one up, `scripts/build-sandbox-
  image.sh` builds the image via the dev container's existing (previously
  unused) DinD sidecar, `docker save`s it, and a new standalone binary
  (`src/bin/sandbox_image_import.rs`) streams the tarball into a
  short-lived pod with the node's containerd socket `hostPath`-mounted and
  runs `ctr images import` — verified with a real verification pod
  reaching `Running` with `imagePullPolicy: Never`, `imageID` matching the
  import exactly. A manual step (`docs/setup.md`), not a new
  `docker-compose.yml` service — it needs a live cluster to do anything at
  all, so it belongs after `docker compose up`, not in its dependency
  graph. One explicit new CI step covers the automated side.
- **Generic, reusable sandbox volumes** — `sandbox_volumes` table (`id`,
  `name`, `mount_path`), PVC-backed, mounted into *every* sandbox pod
  unconditionally via `build_pod_spec`. A leading `~` in the path is
  expanded to the sandbox user's home directory once, at creation.
  `create_volume`/`delete_volume` manage the row and its PVC together
  (rollback on PVC-create failure); `/sandbox-volumes` (index + new page,
  inline arm/confirm delete) is the UI, mirroring `/mcp-servers`'
  shape. Deliberately scoped down from the original idea during planning:
  no upload UI, no single-file/Secret-backed kind, no browsing — a volume
  starts empty and only ever gets content from the model's own
  `write_file`/`edit_file` tools once mounted into a live pod. Both defer
  to a later pass.

**Verification:** 216 `cargo test --features server` tests passing (30 in
`sandbox::`, including 3 new: non-root+sudo identity, and the folded-in
volume-lifecycle assertions inside `test_terminal_lifecycle_end_to_end`),
both build targets clean, and the `/sandbox-volumes` create → list →
delete flow driven through a real browser (`scripts/browser-check/`) end
to end, including confirming the backing PVC actually disappears on
delete.

## Retrospective

**What worked:**
- **Spiking every genuinely risky assumption against the real cluster
  before committing to a design** — registry-free delivery, PID 1 signal
  handling, PVC access mode — caught real, specific findings (not just
  "confirmed it works") that materially shaped the plan: the exact
  `BrokenPipe`-on-zero-stdout gotcha below, `ReadWriteMany` being
  unprovisionable at all on this cluster's `local-path` `StorageClass`
  (not just "assume `ReadWriteOnce`"), and the PID 1 signal-handling
  reasoning being *correct* but still worth the real proof per this
  project's own rule. None of these would have surfaced from reasoning
  alone.
- **Running the real test suite immediately after each structural change**
  caught a real bug within minutes of introducing it: `build_pod_spec`
  never setting `imagePullPolicy: Never`, so Kubernetes' `:latest`-tag
  default (`Always`) tried a real Docker Hub pull for an image that only
  ever exists on the node — every sandbox-creating test failed with
  `ImagePullBackOff` until this was caught and fixed.
- **The idea-and-plan conversation itself absorbed most of the design
  churn before any code existed** — the registry design was proposed,
  spiked, and replaced with the no-registry `ctr import` approach entirely
  during planning; volumes went from "user-chosen kind, upload UI,
  browsing" down to "just directories, no upload" through several rounds
  of the user narrowing scope. Implementation itself was comparatively
  uneventful because of this.

**What caused friction, surprise, or rework:**
- **Streaming a multi-MB payload into a pod via `pods.exec` stdin
  reliably broke (`BrokenPipe`) whenever the executed command produced
  *zero* stdout output**, confirmed by bisection (identical payload,
  command with vs. without a trailing `echo`) — not something `sandbox.rs`
  or the docs had ever noted, and a latent hazard in the (now-removed)
  `inject_and_launch` too. Documented in `docs/testing.md`.
- **A PVC still mounted by a pod carries Kubernetes' own
  `kubernetes.io/pvc-protection` finalizer** — `delete_volume`'s PVC
  delete call succeeding doesn't mean the object is actually gone; the
  first version of the volume-lifecycle test asserted that immediately and
  failed, needing a poll for the pod to actually disappear *before*
  deleting the volume, and a poll for the PVC to disappear rather than a
  single immediate check.
- **`src/bin/sandbox_image_import.rs` couldn't literally reuse
  `sandbox.rs`'s `Sandbox`/`SandboxManager`**, despite the plan's own
  phrasing ("built on the existing create/exec/delete primitives") —
  this crate has no `lib.rs`, so a `src/bin/*.rs` binary is a separate
  crate root with no access to `main.rs`'s private module tree, the same
  reason `sandbox_agent.rs` has always been fully standalone. The *shape*
  was reused; the code itself had to be duplicated. Worth remembering
  before assuming "reuse X" is literal for anything under `src/bin/`.
- **A CSS edit while `dx serve` was already running didn't reach a fresh
  page load** during browser verification — `asset!()`'s content-hashed
  bundle path only gets hot-patched for already-open tabs; a new browser
  instance (what a screenshot script launches every run) can get a stale
  pre-edit bundle. Needed a real `dx serve` restart (killing both the
  wrapper *and* its child server process — the same two-PID gotcha this
  project's docs already warned about) to see the real result. Documented
  in `docs/testing.md`.
- **No Playwright available in this particular environment**, unlike what
  `docs/testing.md` assumes is baked into the dev image — fell back to
  `scripts/browser-check/` (the documented fallback for exactly this
  case), which worked without incident.

**What to change:**
- `docs/migrations.md`'s "Current migrations" table had drifted stale
  across several past features — it listed only the original 3
  migrations, missing (at least) the sandbox pod/terminal, MCP, and
  `sandbox_volumes` migrations, reading as a completeness claim it hadn't
  backed up for a while. Proposed removing the whole file rather than just
  the table, since the rest of it (naming convention, Postgres notes)
  wasn't earning its keep as a separate doc either — the user agreed;
  applied. The three still-current docs that linked to it
  (`development-process.md`, `database.md`, `models.md`) had their
  dangling references removed too, and `CLAUDE.md`'s docs index lost its
  row.
