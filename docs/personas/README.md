# Personas

A persona is a role a Claude instance takes on: what it's responsible for, which ticket statuses it moves, and where it stops. The user starts one by asking an instance to "take on the persona of Project Manager" (or another). **Every sub-agent is started as a persona**: its prompt names the persona, points at this section, and gives only what that job needs (the ticket id, its worktree and branch, the PR). A sub-agent never gets a fork of its starter's conversation. When work comes up that no persona covers, add one here, in the same shape, before handing it out.

Every persona still follows the rest of the [development process](../development-process.md): the plan phase, TDD, the gate, close-out, retro notes and the retrospective. A persona says who does each part, and where a persona's rule differs from another part of the process, the persona's rule wins for the tickets the Project Manager runs.

| Persona | Works on | Moves the ticket | File |
| -- | -- | -- | -- |
| Project Manager | the one project the user directs it to | Backlog to Up Next, within that project | [project-manager.md](project-manager.md) |
| Architect | one project: its description and its breakdown into tickets | creates tickets in Backlog | [architect.md](architect.md) |
| Planner | one Up Next ticket | to Planned, or Todo with no questions | [planner.md](planner.md) |
| Developer | one Todo ticket | to In Progress, then In Review, or Approved with no questions | [developer.md](developer.md) |
| Code Reviewer | one PR | none: it reports to the Developer | [code-reviewer.md](code-reviewer.md) |
| Integrator | one Approved ticket | to Done | [integrator.md](integrator.md) |
| Facilitator | the "Next retrospective" ticket | to In Review, or Approved with no questions | [facilitator.md](facilitator.md) |

Every persona follows [Rules every persona follows](rules.md). Ownership is recorded in Linear as a `started` comment either way the seat takes: `worktree <path>, branch <branch>`, or `conversation <id>, clone <path>` when the persona runs in a conversation (rules.md, "Working from a conversation sandbox"; SME-254, proposal 13).
