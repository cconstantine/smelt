# Development Process

These rules are mandatory. Follow them in order. Do not skip steps.

The process is split into a file per part, so an agent reads only the part its job needs.

| Part | File |
| -- | -- |
| Personas: who does what, and the statuses each moves | [personas/README.md](personas/README.md) |
| Rules every persona follows (ownership, worktrees, the cluster lock, hands-on checks, questions for the user) | [personas/rules.md](personas/rules.md) |
| Project Manager, Architect, Planner, Developer, Code Reviewer, Integrator | [personas/](personas/README.md) |
| Phase 1: the plan | [process/plan.md](process/plan.md) |
| Phase 2: test-first implementation, and the testing rules | [process/tdd.md](process/tdd.md) |
| Rules for the code: outside APIs, spikes, Kubernetes, async boundaries, errors | [process/rules-code.md](process/rules-code.md) |
| Rules for commits, gates and scope | [process/rules-commits.md](process/rules-commits.md) |
| Rules for running processes, servers and scripts | [process/rules-running.md](process/rules-running.md) |
| Definition of done, and adding a feature | [process/done.md](process/done.md) |
| Close-out: Linear, the code review, the retrospective | [process/close-out.md](process/close-out.md) |
| Bug bashes and design reviews | [process/sweeps.md](process/sweeps.md) |

---

## Where work is tracked

Ideas, plans and finished work live in Linear, not in the repo: team **Smelt Agent** (ticket ids `SME-N`). Use the Linear MCP tools to read and update them.

**Every ticket belongs to a project,** one per line of work: a feature, a bug bash, a design review, a cleanup. A project's description in Linear says its goal, its scope, when it's done and the tickets it breaks into, each an atomic unit of work (one plan, one branch, one PR); the [Architect](personas/architect.md) writes it. A ticket that fits no line of work goes in the standing **Maintenance** project. A ticket filed during other work goes in the project of the ticket that found it, and in Maintenance only when it's unrelated (see [Rules every persona follows](personas/rules.md)). The user directs the [Project Manager](personas/project-manager.md) to one project at a time.

**Linear holds tracking; documents live in the repo.** Tickets (with their plans, What shipped and, on a project's retrospective ticket, the retrospective), comments (retro notes among them, on the project), statuses, relations and project descriptions are in Linear. Anything that describes the product or the process and outlives one ticket is a repo file: the Current state docs, the Feature checklist and these process docs. A plan too long for the Linear MCP to return goes in `docs/plans/` on the ticket's branch (see [Planner](personas/planner.md)) and is removed before the PR merges.

| Status | Meaning |
|---|---|
| **Backlog** | An idea: what the user wants, not yet planned. |
| **Up Next** | The user wants it planned next. A Planner plans it (see [Personas](personas/README.md)), unless it's waiting on another ticket (a "blocked by" relation that isn't Done; see [Planning waits for overlapping work](personas/project-manager.md#planning-waits-for-overlapping-work)). |
| **Planned** | Has a plan with questions for the user (see [Questions for the user](personas/rules.md)); the user may leave notes on the ticket. A plan with no questions goes straight to Todo. |
| **Todo** | The user approved the plan; ready to be worked on. |
| **In Progress** | Being implemented on a branch. |
| **In Review** | PR open, reviewed and fixed (or a retrospective written); waiting for the user to answer its questions. A ticket with no questions goes straight to Approved. |
| **Approved** | The user answered the questions and said which recommendations they disagree with, or there were no questions; ready to act on the answers and merge. |
| **Done** | Merged and closed out: the ticket records what shipped. |
| **Canceled** / **Duplicate** | Dropped, or folded into another ticket. |

Each ticket gets one label: **Feature**, **Improvement** or **Bug**. Related tickets are linked as related rather than only mentioned.

The **Current state** docs (`docs/current-state/`, one file per section — index.md first) describe current features, architecture and goals. The **Feature checklist** (`docs/feature-checklist/`, one file per area — index.md first) lists every shipped feature with the steps to check it through the web UI, one row per ticket.

Linear's markdown differs from GitHub's in ways that bite: don't hard-wrap lines (a wrapped line starting with `+` or `-` becomes a list item), put file names in backticks (Linear turns a bare `name.rs` into a web link), and link repo files by their GitHub URL, not a relative path.

---

## Evolving this process

This document should change as we learn what works. Either party can propose a change at any time — the project's retrospective is the natural place (see [Retrospective](process/close-out.md#retrospective-end-of-each-project)), but don't wait.

**Proactively propose changes** when you notice:
- A step that caused unnecessary friction or delay
- A pattern that worked especially well and is not captured here
- A rule that did not fit the situation

To propose a change: describe it in plain text and explain why. No plan doc needed. Once confirmed, update the file it belongs in.

Process changes follow the same confirm-before-change rule — propose first, update after the user agrees.

---

## Writing idea tickets

Idea tickets describe **what the user will be able to do** or **what problem gets solved** — not how it will be built. Keep them abstract and user-focused.

A good idea ticket answers:
- What can the user do that they cannot do today?
- What problem or friction does this remove?

A good idea ticket does **not** include:
- Implementation approach, data models, or API design
- File names, module structure, or technology choices
- Anything that belongs in a plan

Implementation details belong in the plan, which is written once the idea is approved and work begins. If an idea ticket starts to look like a plan, trim it back.

**Example of what to avoid:** "Add a `tool_calls` table with columns `id`, `message_id`, `name`, `input`, `result` and expose it via a new server function `get_tool_calls()`..."

**Example of the right level:** "Smelt can look things up on the web instead of only answering from what it already knows."

---

## Where the old sections went

Links into this file from tickets, briefs and memories point at the headings below, each of which says where its section moved (SME-163).

## Personas

Moved to [personas/README.md](personas/README.md).

### Rules every persona follows

Moved to [personas/rules.md](personas/rules.md).

### Project Manager

Moved to [personas/project-manager.md](personas/project-manager.md).

#### Planning waits for overlapping work

Moved to [personas/project-manager.md](personas/project-manager.md#planning-waits-for-overlapping-work).

### Planner

Moved to [personas/planner.md](personas/planner.md).

### Developer

Moved to [personas/developer.md](personas/developer.md).

### Code Reviewer

Moved to [personas/code-reviewer.md](personas/code-reviewer.md).

### Integrator

Moved to [personas/integrator.md](personas/integrator.md).

## Phase 1: Plan

Moved to [process/plan.md](process/plan.md).

## Phase 2: Implementation (TDD)

Moved to [process/tdd.md](process/tdd.md).

### Step 1 — Write a failing test

Moved to [process/tdd.md](process/tdd.md#step-1--write-a-failing-test).

### Step 2 — Write the minimum code to make it pass

Moved to [process/tdd.md](process/tdd.md#step-2--write-the-minimum-code-to-make-it-pass).

### Step 3 — Refactor if needed

Moved to [process/tdd.md](process/tdd.md#step-3--refactor-if-needed).

### Step 4 — Repeat

Moved to [process/tdd.md](process/tdd.md#step-4--repeat).

## Rules

Split by subject: [process/tdd.md](process/tdd.md#testing-rules) (testing), [process/rules-code.md](process/rules-code.md), [process/rules-commits.md](process/rules-commits.md) and [process/rules-running.md](process/rules-running.md).

## Definition of done

Moved to [process/done.md](process/done.md).

## Adding a New Feature (Typical Flow)

Moved to [process/done.md](process/done.md#adding-a-new-feature-typical-flow).

## Keeping Linear current

Moved to [process/close-out.md](process/close-out.md#keeping-linear-current).

## Retrospective (end of each project)

Moved to [process/close-out.md](process/close-out.md#retrospective-end-of-each-project).

## Code review (after opening the PR)

Moved to [process/close-out.md](process/close-out.md#code-review-after-opening-the-pr).

## Bug bash (every few projects)

Moved to [process/sweeps.md](process/sweeps.md#bug-bash-every-few-projects).

## Design review (every few projects, alternating with bug bashes)

Moved to [process/sweeps.md](process/sweeps.md#design-review-every-few-projects-alternating-with-bug-bashes).
