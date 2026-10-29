# Integrator
Part of the [development process](../development-process.md).
smelt's machine persona file is `.agents/personas/integrator.md`; its `smelt.tools` encode what this page says the persona may do — the only persona rules that allow a merge.


Finishes one **Approved** ticket:

1. Reads the user's answers to the questions and acts on them on the branch.
2. Fetches `origin/main`. If it has not moved since the PR's head was last gated, CI on that head *is* the gate: there is no local re-run, and the report says so with both shas. If it has moved, merges it into the branch (not a rebase: the PR is open, and force-pushing it is ruled out), pushes, and runs the gate again (`scripts/check.sh`, which takes the cluster lock itself). When the PR adds a lint or a gate check, that run covers it on the merged tree: code from `main` wasn't written against the new rule. Merge with a merge commit, not a squash: the per-finding "SME-N review R, finding M" commits are the audit trail (SME-254's Q2).
3. Merges the PR once CI passes on that head. The merge never moves the ticket — status at merge is a seat/human decision, and the Linear↔GitHub integration is expected to be configured not to advance it (see [close-out.md](../process/close-out.md#keeping-linear-current)) — so after merging it re-reads the issue's status after a few seconds (the first read is not trustworthy) and leaves the ticket at **In Review** for the user's close-out. Then it cleans up: on the user's machine, it removes the branch's worktrees; from a conversation sandbox it removes its own checkout and names the worktrees still on the user's machine in its report (SME-254, finding 1 — from a sandbox this step was silently un-doable).

**A retrospective ticket** has no branch until the Integrator makes one. It acts on every proposal the user didn't mark as disagreed: a process or doc change goes on a new branch (see [Rules every persona follows](rules.md)) and into one PR, which gets one round of [code review](../process/close-out.md#code-review-after-opening-the-pr) and merges once CI passes; a code change becomes a Backlog ticket in the project the notes behind it came from (or in Maintenance). It records the declined proposals in the ticket and moves it to **Done**.
