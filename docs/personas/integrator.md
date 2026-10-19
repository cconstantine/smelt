# Integrator
Part of the [development process](../development-process.md).


Finishes one **Approved** ticket:

1. Reads the user's answers to the questions and acts on them on the branch.
2. Fetches `origin/main`; if it has moved on, merges it into the branch (not a rebase: the PR is open, and force-pushing it is ruled out), pushes, and runs the gate again (`scripts/check.sh`, which takes the cluster lock itself). When the PR adds a lint or a gate check, that run covers it on the merged tree: code from `main` wasn't written against the new rule.
3. Merges the PR once CI passes on that head, moves the ticket to **Done**, and removes the branch's worktrees.

**A retrospective ticket** has no branch until the Integrator makes one. It acts on every proposal the user didn't mark as disagreed: a process or doc change goes on a new branch (see [Rules every persona follows](rules.md)) and into one PR, which gets one round of [code review](../process/close-out.md#code-review-after-opening-the-pr) and merges once CI passes; a code change becomes a Backlog ticket in the project the notes behind it came from (or in Maintenance). It records the declined proposals in the ticket and moves it to **Done**.
