# Sweeps: bug bashes and design reviews
Part of the [development process](../development-process.md).

## Bug bash (every few projects)

Every three completed projects or so, run a bug bash. **A bug bash is a project of its own** (named "Bug bash N"): its sweep is one ticket in it, with its own branch and PR, and every finding it files as a ticket goes in that project too, so what's left of a bash is one list. The sweep is: code sweeps of the areas most recently changed, plus hands-on sweeps of the running app with a real model. Include at least one sweep with two tabs open on the same conversation and a reload mid-reply. Most of what the 2026-09-24 bug bash found sat in flows no single project owned (a second tab, a reload, a background reply, a deleted conversation), so no project's own tests covered them. Every bug bash also runs the whole **Feature checklist** through the web UI (including its busy-session row, with every side panel open at once), marking each row works, broken, model (the model didn't cooperate) or not web-checkable, so "does everything still work" is a list to re-run rather than something to reconstruct. Hands-on checks go through the browser only: no seeding the database or calling APIs directly. Run them against `scripts/check-server`, and drive the browser with `scripts/ui-check/smelt_ui.py` (see [testing/browser.md](../testing/browser.md#playwright-preferred)). Write the findings up in the bug bash's ticket, each with a severity and marked confirmed in the UI, confirmed by code, or suspected; stop for the user to choose what gets fixed; then fix them in severity order, one commit per finding, test-first. **When the user picks more than about six fixes, the fixing is two sub-agent runs, not one:** first the fixes and their before/after re-shoots, then the close-out, the PR and the reviews. The plan says so when it splits the bash into parts, and the Project Manager starts the second run once the first reports. SME-122's single fix part carried eleven fixes, 27 Backlog tickets, the close-out, a before/after session pair and two review rounds. See [SME-23](https://linear.app/smelt-agent/issue/SME-23) and [SME-40](https://linear.app/smelt-agent/issue/SME-40).

A bug bash's notes go to the next [retrospective](close-out.md#retrospective) like any others; its sweep is where the process's misses show most plainly, so its personas note what the sweep caught and why it had been missed.

Each retrospective counts the projects completed since the last bug bash, and says so if one is due.

## Design review (every few projects, alternating with bug bashes)

Every six completed projects or so, alternating with bug bashes so one kind of review happens about every three projects, run a design review. Like a bug bash, **a design review is a project of its own** ("Design review N"): the review is one ticket in it, and every finding filed as a ticket goes in that project. The Feature checklist says what should work; a design review asks whether it's good to use. Everything goes through the web UI with a real model, at a laptop width and a phone width, in five passes:

1. **Visual craft**, page by page: type hierarchy, colour and what it means, consistency of repeated elements, spacing, empty states, focus, phone width. Include a **busy session**: every side panel open at once (todos, live browser, sandbox with a terminal), at both widths. Panels are rarely alone in real use; SME-41 reviewed them one at a time and missed that together they squeezed the browser to 100px and the terminal off screen.
2. **Copy**: things named from the user's side, not the system's; buttons that say what happens; errors that say what went wrong and how to fix it.
3. **Information design**: summary before detail, state shown in form and not only in text, interactive things that look interactive.
4. **Workflows**, walked end to end: starting a task, following a long one, stepping in, coming back later, setting up, browsing together, housekeeping.
5. **Information availability**: at each point in those workflows, can the user tell what the model is doing, what it changed, why something failed, how close the conversation is to its limit, and what's running?

Write the findings up in the ticket, each with a category, a severity, evidence (a screenshot or exact steps) and a recommendation, then stop for the user to triage. Small polish the user picks is fixed in one PR, one commit each; anything bigger becomes its own Backlog ticket in the design review's project. Like a bug bash, it ends with a retrospective. See [SME-41](https://linear.app/smelt-agent/issue/SME-41).

At each project's close-out, say whether a design review is due as well.
