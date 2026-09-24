# A multi-hunk / multi-file patch tool

## What

A `patch` tool that applies a unified-diff-style patch — potentially
several hunks, potentially across several files — in one tool call, as an
alternative to `edit_file`'s one `old_string`→`new_string` replacement per
call. Same shape as opencode's `patch` tool.

## Why

`edit_file` is deliberately narrow (see
`docs/projects/completed/20260816-file-tools.md`): one targeted
replacement, refused if the file changed since it was last read. That's
the right default for a single, reviewable edit — it's also why every edit
renders as a real line-level diff in the transcript. But a change that
touches several files (a rename that updates every call site, a refactor
that threads a new parameter through) currently costs one `edit_file` call
per site, each a separate model round trip. A `patch` tool that accepts a
multi-file diff and applies it atomically would collapse that into one
call, while still rendering as the same kind of line-level diff in the
transcript that `edit_file` already produces.

## Depends on

- `similar` (already a dependency, used for `edit_file`'s diff rendering —
  see `Cargo.toml`'s comment on why it's unconditional, not
  server-feature-gated) can likely also parse/apply a unified diff, or at
  least render one; worth checking before reaching for a separate crate.
- The same staleness check `edit_file`/`write_file` already do (content
  hash from the last read) needs a per-file answer when a patch touches
  several files at once: does the whole patch fail if *any* one file has
  drifted, or does it apply file-by-file? All-or-nothing is simpler to
  reason about and matches how a single `edit_file` call already behaves.
- Transcript rendering: `frontend/pages/chat.rs` already renders a single
  `edit_file` call as a diff (see its diff-rendering code) — a
  multi-file patch needs a rendering that's still legible as "N files
  changed" rather than one undifferentiated diff blob.

## Open questions

- Is this actually worth it once `glob`/`grep` land (see
  `docs/projects/ideas/glob-and-grep.md`) and the model can already find
  every call site itself? A multi-file patch mostly saves round trips, not
  new capability — worth sizing the win against the added surface (patch
  parsing/application edge cases: fuzzy matching, partial application,
  conflicting hunks) before committing to it.
- Partial-failure UX: if hunk 3 of 5 fails to apply cleanly, does the tool
  roll back the first two, or leave them applied and report which hunk
  failed?
