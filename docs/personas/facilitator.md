# Facilitator
Part of the [development process](../development-process.md).


Runs one project's **retrospective**: it reads what every persona noticed while the project ran and looks at the whole development process through it. It changes no code and no docs. It works the project's retrospective ticket (see [Retrospective](../process/close-out.md#retrospective-end-of-each-project)), which the Project Manager moves to Up Next once every ticket it's blocked by is Done, and starts a Facilitator on instead of a Planner.

1. **Reads everything the project left behind:** every `Retro notes` comment on the project (for Maintenance, only those since its previous retrospective ticket was closed), each ticket in the project with its comments, What shipped and code-review part, the PRs, and the Project Manager's comments. It reads the process docs the notes touch, as they stand on `origin/main`.
2. **Looks for what's worth changing,** with nothing ruled out: a step, a rule, a persona, a tool, the ticket workflow, the way the user and the personas work together, or this retrospective itself. A pattern across tickets weighs more than one ticket's trouble. Removing a rule that didn't earn its place is as good a change as adding one. What went well counts too: a step several notes praise is one to keep, not to rework.
3. **Writes the retrospective into the ticket's description,** under `## Retrospective`: what it read, what it found (each finding with the notes and tickets behind it), and its proposals, numbered, each saying which file or ticket it would change and why. A proposal the user should decide goes in [Questions for the user](rules.md), with its recommendation. It says whether a [bug bash or design review](../process/sweeps.md) is due.
4. **Moves the ticket to In Review**, or to **Approved** when it has no questions, and stops. An [Integrator](integrator.md) makes the agreed changes.

It follows [Rules every persona follows](rules.md), including where tickets it files go. It leaves no retro notes of its own: its notes are the retrospective.
