# Delete conversations

**Branch:** `delete-conversations` · **Idea:** `projects/ideas/delete-conversations.md` (removed) · **Plan:** `projects/plans/delete-conversations.md` (removed)

## What shipped

The user can delete a conversation from the sidebar: click delete once to
arm it, click again to confirm — no modal, no undo (hard delete). Deleting
the currently-open conversation falls back to the existing empty state.

- `db::delete_conversation` — plain `DELETE`, cascades to the
  conversation's messages via the FK that already had `ON DELETE CASCADE`
  (no new migration needed), treats a nonexistent id as a no-op.
- `DELETE /api/conversations/{id}` server function.
- Sidebar UI: a single `pending_delete` signal (not per-row state) drives
  the arm/confirm interaction.
- A follow-on visual pass on `assets/chat.css`, prompted by the delete
  button shipping with no layout at all (see Retrospective).

## Retrospective

**What worked:**
- TDD on the db layer was smooth and caught something real: the first
  version of `delete_conversation` was a literal stub, and
  `test_delete_conversation_removes_it_from_list` failed against it for
  the right reason before the real query was written.
- Confirming the two open questions from the idea doc (confirm UX,
  hard-vs-soft delete) before writing the plan meant the plan had zero
  open questions left in it — nothing to re-litigate mid-implementation.
- The "mechanical mirror" exception in `development-process.md` was a
  good fit for the cascade/no-op tests — they exercise the same one-line
  query as the first test, and writing them as pure characterization
  tests rather than staging a fake failing version each time was the
  honest call.

**What caused friction / surprises:**
- The sidebar delete button shipped with **zero CSS** and looked
  actively broken (title text ran straight into the button, no gap) —
  nothing in the test suite or the compile step catches "new DOM element,
  no styling," since there's no automated frontend tier
  (`docs/testing.md`'s "What's not covered yet"). It only surfaced
  because of an explicit follow-up ask for a visual pass.
- No browser tool was connected this session, so verifying the UI meant
  building throwaway tooling: download a Chrome-for-Testing zip, resolve
  its ~16 missing shared libraries via non-root `apt-get`
  (`-o Dir::State::lists=...` pointed at a scratch dir) + `dpkg-deb -x`
  extraction, and hand-roll a CDP driver over Node's built-in `fetch` /
  `WebSocket` globals. It worked well once built, but it's real setup
  cost that would be paid again next time a UI change needs visual
  verification in this environment.
- PR creation hit a hard stop: `gh` isn't installed and there's no
  `GITHUB_TOKEN` in the environment. There is a VS Code-provided git
  credential helper that likely has one, but reading a credential out of
  it to hand to `gh`/the API is exactly the kind of secret-exfiltration
  action the permission classifier correctly blocks — so this session
  can push a branch over the already-authenticated SSH remote, but
  cannot open the actual PR without the user's help.

**What to change (proposals — not yet applied):**
- Consider documenting the Chrome-for-Testing + non-root-`apt-get` +
  `dpkg-deb` recipe (or committing the small CDP driver script) somewhere
  under `docs/testing.md`, so the next UI-visual-check doesn't start from
  zero.
- Consider adding `gh` to the dev container image, or documenting a
  supported way to authenticate it, if PR creation from inside the
  container is something we want to keep doing this way.
- When a plan adds a new interactive element to an existing component
  (not a new page), consider calling out "needs at least stub styling in
  the same change" explicitly — nothing currently catches an unstyled
  element until a human (or a headless-browser pass) looks at it.
