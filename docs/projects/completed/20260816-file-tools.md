# File read/write/edit tools, and one pod per conversation

**Branch:** `file-tools` · **Idea:** `projects/ideas/coding-session.md` (file read/write tools and file-edit diffs are now shipped — the idea file's remaining open items are a coding-oriented system prompt and `git clone`/credential wiring) · **Plan:** `projects/plans/file-tools.md` (removed)

## What shipped

Three new file tools, matching the shape modern coding agents (Claude
Code's own Read/Edit/Write among them) have converged on, plus a fourth
for directory listing, plus a scope-simplifying constraint that made all
of it easier to build correctly:

- **`read_file`** — paginated (`offset`/`limit`, line-based), line-numbered
  output, so a huge file doesn't have to be consumed into context at
  once. Returns a SHA-256 hash of the *full* file (not just the returned
  slice) alongside the content.
- **`edit_file`** — a targeted `old_string`→`new_string` replacement, not
  a whole-file rewrite. Both strings can span multiple lines. `old_string`
  must match exactly once (or the call fails, asking for more context)
  unless `replace_all` is set; for identical repeated blocks no amount of
  context can disambiguate, `expected_line` targets one specific
  occurrence by its line number instead (mutually exclusive with
  `replace_all`).
- **`write_file`** — create a new file, or fully overwrite an existing
  one when a targeted edit isn't the right shape.
- **`list_directory`** — non-recursive listing (name, file/dir, byte
  size), so the model can discover what exists without a terminal
  round-trip through `ls`.
- **Read-before-write discipline with real staleness detection, not just
  "was it read."** `edit_file`, and `write_file` when overwriting, require
  the target path was `read_file`'d (or written/edited) earlier in the
  conversation — but a terminal command in the same pod can change a file
  between a read and a later edit, so every `read_file`/`edit_file`/
  `write_file` result carries a content hash, and `edit_file`/overwriting
  `write_file` compare the file's *current* hash against the last known
  one before ever touching it, refusing (not silently clobbering) on a
  mismatch. Point-in-time compare-at-write-time, not a filesystem watch —
  nothing in this turn-based architecture could act on a watch
  notification mid-turn anyway.
- **One pod per conversation.** `create_pod` now refuses if one already
  exists (`terminate_pod` first); every other pod-scoped tool
  (`create_terminal`, the four file tools, `terminate_pod` itself)
  dropped `pod_id` as a parameter entirely, resolving "the conversation's
  pod" implicitly via a new `conversation_pod_id` helper. This is what
  let the file tools avoid ever needing a `pod_id` of their own, and
  removed the sandbox panel's pod tab bar (unreachable once a
  conversation can't have more than one live pod) — a real capability
  given up (isolating an experiment, running two environments side by
  side) for fewer parameters the model has to track correctly on every
  call.
- **Diff rendering.** `edit_file`'s `old_string`/`new_string` already
  carry everything a diff needs — the transcript renders a real
  line-level diff (`similar::TextDiff`, not a naive
  all-old-removed/all-new-added) instead of a generic tool-call card.

**Verification:** 165 `cargo test --features server` tests (the real-
cluster integration test extended to cover the one-pod guard and all four
file tools end to end — hash staleness, `expected_line`, `replace_all`,
the size cap, directory listing), WASM check clean, and the automated
browser test (adjusted for the tab bar's removal).

**Also on this branch** (mid-session requests, not part of the original
plan — see the retrospective on why these stayed separate commits rather
than silently folding into "file-tools"):
- A real, live bug found by inspecting a real conversation: terminal
  commands never actively woke the model when they finished, only
  getting picked up whenever something *else* happened to touch the
  conversation. Fixed and shipped separately as its own project — see
  [20260816-terminal-exit-notify.md](20260816-terminal-exit-notify.md).
- `ANTHROPIC_AUTH_TOKEN` support (bearer auth, for an Anthropic-compatible
  gateway like Hugging Face's hosted endpoint, as an alternative to
  `ANTHROPIC_API_KEY`) — the user's own start, finished: the original
  version still unconditionally required `ANTHROPIC_API_KEY` even when
  only an auth token was configured, defeating the point.
- Sandbox pods' `512Mi`/`500m` CPU/memory limit removed outright (not
  raised) as an interim fix, after a real spike against the cluster found
  reintroducing it properly needs real OOM detection/attribution work of
  its own — see `projects/plans/sandbox-oom.md` (on hold).

Explicitly not done (per the idea file's still-open items): a
coding-oriented `system` prompt, `git clone`/credential wiring.

## Retrospective

**What worked:**
- **Iterating on the plan's shape before writing any code** (state-of-the-
  art Read/Edit/Write vs. a primitive read/write pair, one-pod-per-
  conversation, the content-hash staleness design) meant Phase 2 had very
  few design surprises — most decisions were already settled by the time
  implementation started.
- **Running the real-cluster integration test early and often caught two
  genuinely significant bugs no amount of reasoning would have**: the
  browser test's tab-bar assumption breaking under one-pod-per-
  conversation, and — much more consequentially — discovering that
  `ANTHROPIC_API_KEY` is genuinely present in this environment's real
  process env, which meant `terminal-exit-notify`'s active-wake wiring
  was about to fire real, uncontrolled, hanging requests against the live
  Anthropic API from inside this file-tools branch's own test suite.
  Caught by actually running the test, not by inspecting the code.
- **The plan doc stayed current throughout** — every mid-conversation
  design change (list_directory, `expected_line`, the exact read-before-
  write semantics) got folded back into the plan file itself before
  implementation, not just discussed and forgotten. `sandbox-visibility`'s
  own retrospective flagged the plan going stale as scope grew; this
  branch is the first real evidence that lesson stuck.
- **Rebasing onto `terminal-exit-notify` after it merged separately was
  mostly clean** — only `sandbox.rs`'s own integration test needed manual
  conflict resolution, because the two branches touched mostly disjoint
  regions even within shared files.

**What caused friction, surprise, or rework:**
- **A real inconsistency in the plan itself surfaced once implementation
  started**: "What" said a brand-new `write_file` call needs no prior
  read, but "Which files" described refusing on *any* missing history
  match for both `write_file` and `edit_file`. Resolved in favor of
  "What"'s stated intent (the lower-layer signatures — `write_file`'s
  `Option<String>` `expected_hash` vs. `edit_file`'s required `String` —
  already encoded that resolution) rather than pausing to re-confirm,
  since the plan had already been explicitly approved and the fix was
  unambiguous from context.
- **Two unrelated efforts (a live bug fix, then two more small pieces of
  work) arrived mid-branch**, each requiring a stash-and-switch cycle to
  avoid silently tangling them into file-tools' own history — handled
  correctly each time via `git stash`, but only decided *how* to actually
  package the result (separate commits vs. separate branches vs. one
  commit) at the very end of the session, rather than at the point each
  request arrived.
- **The OOM detection/handling plan turned out more complex once actually
  spiked against the cluster** than it looked on paper — a leaf-process
  OOM kill is invisible at the Kubernetes API level entirely, `dmesg` is
  blocked, and multi-terminal misattribution is a real, unresolved edge
  case. Correctly caught before half-building it, and correctly resolved
  by reducing scope to "just remove the limit for now" rather than
  shipping the partial design — but the spike itself took three attempts
  to actually trigger a real OOM (a growing string was too slow to
  outpace its own timeout; a disk-backed `dd` write never triggered one
  at all, since page cache is reclaimable and the kernel had no reason to
  invoke the OOM killer).

**What to change (confirmed and applied to `development-process.md`):**
- When a fresh, unrelated request arrives mid-branch, decide how to
  package it (separate commit? separate branch?) at the point it
  arrives, not just at the end of the session — the packaging question
  is easy to answer in the moment and gets harder to reconstruct later,
  once several such requests have piled up in the same working tree.
