# Close-out
Part of the [development process](../development-process.md).

What shipped, the docs, the code review and the retrospective: the gate on merging a project's PR.

## Keeping Linear current

After each project completes:
- Replace the ticket's `## Plan` section with `## What shipped` (including anything changed from the plan, and what's not done)
- Update the **Current state** docs if features or architecture changed
- Add or update the ticket's row in the **Feature checklist** (`docs/feature-checklist/`, the file for its area): what a user can do, and how to check it in the browser
- File anything left undone that's worth doing as its own Backlog ticket in the same project, linked as related (see where tickets go in [Rules every persona follows](../personas/rules.md))
- When the project deleted a feature, search for its name in prose too (docs, comments, CSS, prompts, test names), not only its identifiers: the compiler finds the code, nothing finds the sentences. On SME-54, removing `run_async`'s task suite left wording about it in docs and comments that only the code review found.
- Put the ticket id in the PR's title or body
- Review the PR once it's open (see [Code review](#code-review-after-opening-the-pr))
- Once the reviews are done, add `## Retrospective` to the ticket (see below), before the PR merges
- Move the ticket to **Done** once the PR merges

When the user mentions a new idea, create a **Backlog** ticket for it before it is forgotten, in the project it belongs to, or in Maintenance when none fits. An idea too big for one ticket becomes a project of its own instead, with the user's words as its description, for an [Architect](../personas/architect.md) to break down when the user directs the Project Manager to it.

**After a range edit to a Linear ticket or project description** (`replace_range`, or replacing a whole section), read it back and compare it against the version from before the edit; sections outside the range should be unchanged. A save's reply can show stale content, so read it again rather than trusting the reply. On SME-32 a `replace_range` on the Current state document silently deleted 11 architecture bullets, restored only because an earlier copy was still in the session. `replace_range`'s `to` anchor is exclusive and stays in place: end the range at the next section's heading and **don't** repeat that heading in `new_string`, or it appears twice (or `replace` the whole old section). Ending it at a section's own last line left that line duplicated on SME-70, SME-49, SME-86 and SME-90; this doc's old advice to include the `to` heading in `new_string` doubled a heading on SME-117.

**Prefer `replace` with a unique anchor over `replace_range`.** Quote just the text that changes, with enough around it to match once; a range's two anchors are where the doubled lines and lost sections above came from. **When appending a table row** (`append`, or `insert_after` a table's last row), check whether the content already ends with a newline, and if it does, don't start the row with another: a blank line ends a Markdown table, so the new row shows as a paragraph under it. On SME-106 range edits left a doubled heading, and an append left a blank line splitting a table.

**A full-content field on these tools is a whole-body replace, not a section update.** Passing `description` to `save_issue` writes the entire ticket description, so every section left out of the call is deleted — on SME-157 an update meant to add a `## What shipped` section dropped the Evidence section and the user's own answers to three questions, restored only because the pre-edit text happened to be in the session. Patch operations are the only safe form of update here. And when a patch's `old_string` is text the document holds more than once (a list item copied by an earlier failed attempt), include the surrounding lines that make it match once, or the op edits a copy of it: the same attempt to place one question landed three copies and a placeholder across the document before the ops were anchored.

**A section a document holds in one place can become three in a bad edit.** Read back the *whole* changed section after patching, not just the anchor, and compare against the version from before: SME-157's questions list was left with the same question as a stray item above its own heading, inside the list, and after the last section of the document.

## Retrospective (end of each project)

**Close-out (this section plus "Keeping Linear current" above) is a
gate on merging the project's PR, not a follow-up to do after it merges.**
What shipped, the Current state docs and the Feature checklist come
before the PR opens. The retrospective comes after the code review
finishes, since the reviews are often where the most is learned (SME-32's
ten review rounds were, and a retrospective written before them missed
it all).
Do it on the same branch as the implementation, so the close-out commit
rides along in the same PR — not as a separate direct-to-main commit
afterward, which is easy to forget entirely once the PR is merged and
attention has moved to whatever's next. Discovered on `todo-list-tool`:
its close-out was skipped after merge and only caught because the user
noticed the stale plan file still sitting in the repo — checking
"did the last thing get closed out" at the *start* of the next project
relies on remembering to look backward, which is exactly what failed;
gating it at the point the current project's own PR is created doesn't
have that failure mode. The same holds for merging: the retrospective is
the last step before it.

Before considering a project closed, do a short retrospective covering:

- **What worked** that we should keep doing.
- **What caused friction, surprise, or rework** — especially anything discovered
  late that an earlier check would have surfaced.
- **What to change**: concrete proposals to this process, the docs, or the code.

Record it as a short **Retrospective** section in the project's ticket, and say whether a [bug bash](sweeps.md#bug-bash-every-few-projects) is due.
Any resulting process changes follow the [confirm-before-change rule](../development-process.md#evolving-this-process):
propose first, update after the user agrees. On a ticket the Project Manager runs, a recommendation counts as agreed when the user doesn't mark it as disagreed, including when the ticket moves on because it had no questions; one the persona thinks the user should decide goes in Questions for the user.

## Code review (after opening the PR)

Every PR gets a code review once it's open, while CI runs, by a [Code Reviewer](../personas/code-reviewer.md) sub-agent started on the PR. It reads the whole diff for bugs, including security gaps, which a project's own tests are poorly placed to find because they were written with the same assumptions as the code.

Don't run `/code-review` from the conversation that wrote the change: the skill forks that conversation, and its reviewer then inherits the author's assumptions and context, against the rule that sub-agents start fresh (see [Personas](../personas/README.md)). The Code Reviewer may run `/code-review <PR number>` itself, since its fork carries only the Code Reviewer's own brief; it still checks and reports each finding as its persona says. (SME-114)

1. **Check each finding before reporting it.** Confirm it against the code (and, where it's cheap, reproduce it) so the report says which findings are real.
2. **Report the findings and stop.** Give each one a severity, what goes wrong and a suggested fix, and let the user choose what gets fixed. Fixing changes the PR under review, so it waits for their go-ahead.
3. **Fix on the PR's branch, test-first, one commit per finding.** A failing test first, as in Phase 2; for a finding about a browser's behaviour, a browser-tier scenario shown failing with the fix switched off. A finding in already-merged code gets its own PR (see [rules-commits.md](rules-commits.md)). **When fixing a finding, search for every other place that makes the same decision** (the same advice, comparison or check) and fix them in the same commit. On SME-121 round 1 fixed the missing-image advice, and round 2 found the same gap in the outdated-agent advice. **When a round has more than two fixes, gate each from a detached gate worktree while writing the next** (see the gating rule in [rules-commits.md](rules-commits.md)), rather than one after another: SME-121's eleven review fixes, mostly advice text, took eleven full gates of about 20 minutes of lock each, back to back.
4. **Review again after fixing, twice in all by default.** Fixes change the code, and a second pass looks with fresh eyes. The second round follows fixes: when the first round finds nothing to fix, it's the only one (SME-70). After the second round, only high-severity findings block the merge; report the rest and file each one the user wants as a Backlog ticket, linked as related. **Exception: a third round when the second finds medium-severity issues in a new integration** (a new external system, protocol or process smelt drives). Fix the second round's findings first, then review once more; that's the last. On SME-35 (language servers) the second round found two medium issues, and the third found a catalog suggestion that could never start. SME-32 ran ten rounds, each mostly fixing edges made by the last; a round that keeps finding new edges points to a missing state model (see [Phase 1](plan.md)), which another round won't supply.
5. **Record it in the ticket**: a short code-review part in What shipped listing each finding and its fix.
6. **Then write the retrospective** (see [Retrospective](#retrospective-end-of-each-project)), covering the reviews too, and only then merge.

On SME-42, the first review found that both new routes could reach the sandbox agent's command WebSocket (any website could have run commands in the sandbox). The project's own real-cluster tests had used that very port as their test server. The second review, after the fixes, found a stalled POST and cross-site requests to the sandbox. None of the four were caught by the tests, the browser tier or the hands-on check.
