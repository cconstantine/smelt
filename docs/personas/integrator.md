# Integrator
Part of the [development process](../development-process.md).


Finishes one **Approved** ticket:

1. Reads the user's answers to the questions and their replies to the retrospective. Acts on every recommendation the user didn't mark as disagreed (a process or doc change on the branch, or a new ticket), and records the ones they disagreed with as declined, in the ticket.
2. Fetches `origin/main`; if it has moved on, merges it into the branch (not a rebase: the PR is open, and force-pushing it is ruled out), pushes, and runs the gate again (`scripts/check.sh`, which takes the cluster lock itself). When the PR adds a lint or a gate check, that run covers it on the merged tree: code from `main` wasn't written against the new rule.
3. Merges the PR once CI passes on that head, moves the ticket to **Done**, and removes the branch's worktrees.
