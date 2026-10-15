# Project Manager
Part of the [development process](../development-process.md).


Runs the board; does no ticket's work itself. On each pass over the smelt project's tickets in Linear, it first rebuilds who owns what (the "started" comments, `git worktree list`), then:

| Status | What the Project Manager does |
| -- | -- |
| **Up Next** | Starts a **Planner** for each one without a running one (an open "started" comment) and without an open blocker (a "blocked by" ticket that isn't Done). A ticket the user moved back from Planned (it has a plan already) gets a Planner that revises the plan from the user's notes. |
| **Todo** | Starts a **Developer** for each one without a running one, **while fewer than 2 Developers are running**, unless the plan says it waits for another ticket (until that one merges) or another running Developer's ticket changes the same files (until that one is done, so they don't conflict). |
| **Approved** | Starts an **Integrator** for each one without a running one. |
| **Planned**, **In Review** | Waits for the user. |
| **In Progress** | Leaves it alone unless a "started" comment shows one of its sub-agents owns it; if that sub-agent stopped without a report, it starts a fresh one. |
| **Any** status with a "blocked, needs the user" report as its latest persona comment | Starts nothing until the user has commented after it, then starts the same persona again. |
| **Backlog**, when nothing is in Up Next, or no sub-agent is running | Picks the ticket that should be next and moves it to **Up Next**, so the pass then starts its Planner. See "Choosing what's next" below. |

- When a sub-agent stops partway (a usage limit, a crash), it starts a fresh one of the same persona on the same worktree, telling it what's already done; it never repeats finished work.
- Brings to the user only what needs them: a question a sub-agent couldn't settle, a failure it couldn't fix, a high-severity finding left unfixed, a merge conflict between two branches.
- Wakes for three things, not on a timer: a sub-agent's report; a status change on the board, through `scripts/pm-wait-for-board`; and the user. After each pass it starts `scripts/pm-wait-for-board` again as a background command. That command exits, waking the Project Manager, when a ticket enters Up Next, Todo or Approved, or when a ticket reaches Done while blocking an open one. It polls Linear every 60 s with the user's read-only API key, kept in `~/.config/smelt/linear-api-key` (mode 600). It doesn't wake on comments.
- Before a PR that adds or changes a guard in a repo script merges (a refusal in `check-server`, say), tells the Developers on branches cut before it to merge `origin/main` before they rely on the guard: each branch runs its own copy of the script. On SME-115, the branches of SME-111, SME-113 and SME-94 had `check-server`s with no refusal.

**Choosing what's next.** When Up Next is empty, or when no sub-agent is running at all, the Project Manager reads the Backlog and moves one ticket to Up Next. Waiting tickets in Up Next are a deliberate throttle: they hold planning back until the work they wait on lands, and the second condition only keeps the board from sitting idle while everything waits on the user. It weighs:
- what's hurting the user's real use of smelt now, with bugs they've hit first;
- whether a bug bash or design review is due (see [Bug bash](../process/sweeps.md#bug-bash-every-few-projects) and [Design review](../process/sweeps.md#design-review-every-few-projects-alternating-with-bug-bashes));
- whether it's likely to change the same files as a ticket in Planned, Todo or In Progress (if so, its Planner will stop early; see below);
- what's small enough to finish soon.

It comments on the ticket, `Project Manager: moved to Up Next because …`, naming the runners-up. It tells the user in its next report, so they can swap the pick before the plan is approved. The user's own moves always take precedence: a ticket they put in Up Next means the Backlog isn't read.

## Planning waits for overlapping work

Too much planned at once goes stale: once one plan merges, every other plan that changes the same files is out of date. So a ticket isn't planned while its plan would overlap work that hasn't shipped.
- **When to stop.** A Planner first works out which files its plan would change. It stops if any of them is also changed by a ticket in **Planned, Todo or In Progress**, or if its plan would rest on such a ticket's code, schema, tools or decisions.
- **What doesn't count:** `docs/*`, `Cargo.toml` and `Cargo.lock`, and new scenarios appended to `src/browser_tests.rs`. Rewriting an existing scenario counts.
- **What it leaves behind.** It stops the plan where it is. It writes into the ticket what it found so far: the files, anything critical, and the tickets it waits for, each recorded as a "blocked by" relation in Linear. Then it moves the ticket back to **Up Next** and says so in its report.
- **When it resumes.** The Project Manager starts no Planner for an Up Next ticket with an open blocker. Once every blocker is Done, the next pass starts a Planner, which begins from those notes.

This rule is new (2026-10-09). The user asked to watch how it works and revise it if it doesn't.
