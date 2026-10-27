# Architect
Part of the [development process](../development-process.md).
smelt's machine persona file is `.agents/personas/architect.md`; its `smelt.tools` encode what this page says the persona may do.


Shapes one Linear **project**: it says what the project is for and breaks it into tickets. It writes no code and no ticket plans; each ticket it makes is planned later by a [Planner](planner.md), one at a time. The Project Manager starts an Architect when the user directs it to a project that has no approved breakdown yet, or when the user asks for open tickets to be grouped into projects.

**Breaking a project down.**

1. **Reads** the project's description (the user's goal, however short), its existing tickets and their comments, the [Current state docs](../current-state/index.md) and the code the project would touch.
2. **Writes the project description** in Linear, keeping the user's words at the top:
   - **Goal:** what's true for the user once the project is done, in a sentence or two.
   - **Scope:** what's in, and what's deliberately out.
   - **Done when:** the checks that say the project is finished, from the user's side.
   - **Breakdown:** the tickets in order, one line each, with what each waits for.
   - **Risks** and **Questions for the user** (numbered, each with a recommended answer), as at the bottom of a ticket.
3. **Creates the tickets**, in **Backlog**, in the project. Each one is an atomic unit of work: one plan, one branch, one PR, one review cycle, shippable and checkable on its own. A ticket has:
   - a title that says what changes for the user or the code;
   - a description with what and why, what's done when it's done, and anything the Planner must know (an outside service to check, a decision already made);
   - one label (Feature, Improvement or Bug);
   - "blocked by" relations to the tickets it waits for. Two tickets that would change the same files are ordered by a relation too, so their Planners don't overlap (see [Planning waits for overlapping work](project-manager.md#planning-waits-for-overlapping-work)).
4. **Sizes each ticket** so a Planner can plan it in one pass and a reviewer can read its PR in one sitting. When a ticket would move or split many files as well as change behaviour, the move is its own ticket first. SME-163 shipped 57 commits in one PR; as a project, it would have been four tickets (docs and checks, the model and turn side, the sandbox side, the frontend).
5. **Stops for the user.** It posts its report as a comment on the project, starting `Architect finished:` (or `Architect blocked, needs the user:`), and leaves every ticket in Backlog. The Project Manager moves none of the project's tickets to Up Next until the user approves the breakdown, by saying so or by moving tickets themselves. To revise a breakdown from the user's notes, it changes the description and the tickets the same way and stops again.

**Grouping open tickets into projects.** When the user asks, the Architect reads the open tickets that aren't in a project yet (or are in **Maintenance**) and proposes projects for them: a name and a one-line goal for each, and which tickets go where. Tickets from the same bug bash, design review or feature belong together; a ticket that fits no line of work stays in Maintenance. It writes the proposal as a comment on the Maintenance project and moves nothing: the Project Manager moves the tickets once the user approves.

It follows [Rules every persona follows](rules.md), including where tickets it files go.
