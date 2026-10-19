# Close-out
Part of the [development process](../development-process.md).

What shipped, the docs and the code review: the gate on merging a ticket's PR. Then retro notes and the standing retrospective.

## Keeping Linear current

After each project completes:
- Replace the ticket's `## Plan` section with `## What shipped` (including anything changed from the plan, and what's not done)
- Update the **Current state** docs if features or architecture changed
- Add or update the ticket's row in the **Feature checklist** (`docs/feature-checklist/`, the file for its area): what a user can do, and how to check it in the browser
- File anything left undone that's worth doing as its own Backlog ticket in the same project, linked as related (see where tickets go in [Rules every persona follows](../personas/rules.md))
- When the project deleted a feature, search for its name in prose too (docs, comments, CSS, prompts, test names), not only its identifiers: the compiler finds the code, nothing finds the sentences. On SME-54, removing `run_async`'s task suite left wording about it in docs and comments that only the code review found.
- Put the ticket id in the PR's title or body
- Review the PR once it's open (see [Code review](#code-review-after-opening-the-pr))
- Move the ticket to **Done** once the PR merges

When the user mentions a new idea, create a **Backlog** ticket for it before it is forgotten, in the project it belongs to, or in Maintenance when none fits. An idea too big for one ticket becomes a project of its own instead, with the user's words as its description, for an [Architect](../personas/architect.md) to break down when the user directs the Project Manager to it.

**After a range edit to a Linear ticket or project description** (`replace_range`, or replacing a whole section), read it back and compare it against the version from before the edit; sections outside the range should be unchanged. A save's reply can show stale content, so read it again rather than trusting the reply. On SME-32 a `replace_range` on the Current state document silently deleted 11 architecture bullets, restored only because an earlier copy was still in the session. `replace_range`'s `to` anchor is exclusive and stays in place: end the range at the next section's heading and **don't** repeat that heading in `new_string`, or it appears twice (or `replace` the whole old section). Ending it at a section's own last line left that line duplicated on SME-70, SME-49, SME-86 and SME-90; this doc's old advice to include the `to` heading in `new_string` doubled a heading on SME-117.

**Prefer `replace` with a unique anchor over `replace_range`.** Quote just the text that changes, with enough around it to match once; a range's two anchors are where the doubled lines and lost sections above came from. **When appending a table row** (`append`, or `insert_after` a table's last row), check whether the content already ends with a newline, and if it does, don't start the row with another: a blank line ends a Markdown table, so the new row shows as a paragraph under it. On SME-106 range edits left a doubled heading, and an append left a blank line splitting a table.

**A full-content field on these tools is a whole-body replace, not a section update.** Passing `description` to `save_issue` writes the entire ticket description, so every section left out of the call is deleted — on SME-157 an update meant to add a `## What shipped` section dropped the Evidence section and the user's own answers to three questions, restored only because the pre-edit text happened to be in the session. Patch operations are the only safe form of update here. And when a patch's `old_string` is text the document holds more than once (a list item copied by an earlier failed attempt), include the surrounding lines that make it match once, or the op edits a copy of it: the same attempt to place one question landed three copies and a placeholder across the document before the ops were anchored.

**A section a document holds in one place can become three in a bad edit.** Read back the *whole* changed section after patching, not just the anchor, and compare against the version from before: SME-157's questions list was left with the same question as a stray item above its own heading, inside the list, and after the last section of the document.

## Gate on merging

**Close-out ("Keeping Linear current" above, and the code review below) is a gate on merging the ticket's PR, not a follow-up to do after it merges.** What shipped, the Current state docs and the Feature checklist come before the PR opens, on the same branch as the implementation, so they ride along in the same PR. Discovered on `todo-list-tool`: its close-out was skipped after merge and only caught because the user noticed the stale plan file still sitting in the repo. Checking "did the last thing get closed out" at the *start* of the next project relies on remembering to look backward, which is exactly what failed; gating it at the point the current PR is created doesn't have that failure mode.

## Retro notes (while the work runs)

Every persona leaves notes for the retrospective as things happen, while its context is fresh: see "Retro notes" in [Rules every persona follows](../personas/rules.md). They are comments on the standing **Next retrospective** ticket in Maintenance, so one read finds them all, whichever ticket or project they came from. A note written only at the end of a long run reflects whatever survived compaction, so a surprise, a failure, a rescue or a win gets its note when it happens.

## Retrospective

A retrospective looks at the whole development process, across every project, so it doesn't wait for a project to end: projects interleave, grow tickets as they go, and can stop when they're done enough. Before 2026-10-18 each ticket's Developer wrote one, seeing one ticket from one seat; briefly after that each project had one, which never ran for a project that didn't finish.

- **There is always exactly one open "Next retrospective" ticket,** in **Maintenance**, labelled Improvement. Retro notes collect on it as comments.
- **The Project Manager decides when it runs,** and moves it to Up Next when any one of these holds: about 8–10 notes have built up since the last retrospective; about two weeks have passed with at least a few notes; a note says something is hurting right now (a rule that keeps causing rework, say); or the user asks. It says so in its report. It counts notes from the ticket's comments, whichever project it's working.
- **A [Facilitator](../personas/facilitator.md) runs it** when the ticket reaches Up Next, instead of a Planner. It first creates the next "Next retrospective" ticket, so notes posted meanwhile land there, then writes the retrospective into its own ticket: what the notes and tickets showed, and numbered proposals.
- **Process changes follow the [confirm-before-change rule](../development-process.md#evolving-this-process):** the proposals wait for the user at In Review. An [Integrator](../personas/integrator.md) then makes the ones the user didn't mark as disagreed, in one PR, and files a ticket for each one that is code.

## Code review (after opening the PR)

Every PR gets a code review once it's open, while CI runs, by a [Code Reviewer](../personas/code-reviewer.md) sub-agent started on the PR. It reads the whole diff for bugs, including security gaps, which a project's own tests are poorly placed to find because they were written with the same assumptions as the code.

Don't run `/code-review` from the conversation that wrote the change: the skill forks that conversation, and its reviewer then inherits the author's assumptions and context, against the rule that sub-agents start fresh (see [Personas](../personas/README.md)). The Code Reviewer may run `/code-review <PR number>` itself, since its fork carries only the Code Reviewer's own brief; it still checks and reports each finding as its persona says. (SME-114)

1. **Check each finding before reporting it.** Confirm it against the code (and, where it's cheap, reproduce it) so the report says which findings are real.
2. **Report the findings and stop.** Give each one a severity, what goes wrong and a suggested fix, and let the user choose what gets fixed. Fixing changes the PR under review, so it waits for their go-ahead.
3. **Fix on the PR's branch, test-first, one commit per finding.** A failing test first, as in Phase 2; for a finding about a browser's behaviour, a browser-tier scenario shown failing with the fix switched off. A finding in already-merged code gets its own PR (see [rules-commits.md](rules-commits.md)). **When fixing a finding, search for every other place that makes the same decision** (the same advice, comparison or check) and fix them in the same commit. On SME-121 round 1 fixed the missing-image advice, and round 2 found the same gap in the outdated-agent advice. **When a round has more than two fixes, gate each from a detached gate worktree while writing the next** (see the gating rule in [rules-commits.md](rules-commits.md)), rather than one after another: SME-121's eleven review fixes, mostly advice text, took eleven full gates of about 20 minutes of lock each, back to back.
4. **Review again after fixing, twice in all by default.** Fixes change the code, and a second pass looks with fresh eyes. The second round follows fixes: when the first round finds nothing to fix, it's the only one (SME-70). After the second round, only high-severity findings block the merge; report the rest and file each one the user wants as a Backlog ticket, linked as related. **Exception: a third round when the second finds medium-severity issues in a new integration** (a new external system, protocol or process smelt drives). Fix the second round's findings first, then review once more; that's the last. On SME-35 (language servers) the second round found two medium issues, and the third found a catalog suggestion that could never start. SME-32 ran ten rounds, each mostly fixing edges made by the last; a round that keeps finding new edges points to a missing state model (see [Phase 1](plan.md)), which another round won't supply.
5. **Record it in the ticket**: a short code-review part in What shipped listing each finding and its fix.
6. **Leave retro notes** about the reviews too (see [Retro notes](#retro-notes-while-the-work-runs)), then merge.

On SME-42, the first review found that both new routes could reach the sandbox agent's command WebSocket (any website could have run commands in the sandbox). The project's own real-cluster tests had used that very port as their test server. The second review, after the fixes, found a stalled POST and cross-site requests to the sandbox. None of the four were caught by the tests, the browser tier or the hands-on check.
