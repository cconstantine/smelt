# Move off SQLite to Postgres

## What

Smelt's persistence layer moves from SQLite to Postgres — a database that
handles concurrent writers and evolving schema more gracefully, and pairs
naturally with the project's existing `docker-compose` dev/deploy story.

## Why

Two pressures compound over the next several projects, both flowing from
the coding-session work:

- **Concurrency.** "Single user" doesn't mean "single writer" — the
  coding-session idea implies multiple agents/tool-loops can be in flight
  at once, each writing messages and tool state around the same time.
  SQLite's WAL mode tolerates concurrent readers with one writer and can
  likely be tuned further (e.g. `busy_timeout`) before this is a real
  problem, but there's a ceiling to how far that tuning goes.
- **Schema churn.** SQLite's `ALTER TABLE` is limited — no real column
  type/constraint changes without a full table rebuild. The tool-use and
  sandboxing work ahead means real, repeated schema churn (tool-call
  persistence, sandbox-session state, and whatever else those plans need).
  Paying that migration friction on every one of those plans is a real,
  recurring cost. Switching now, while there's little data and few call
  sites touching the database, is cheaper than switching after several
  more schema generations and more real conversation history accumulate.

The trade-off going the other way: SQLite today is a zero-ops embedded
file — no service to run, no connection string, no backup story beyond
"copy the file." Moving to a client/server database gives that up. That
cost is real, though probably smaller here than usual, since the project
already runs everything through `docker-compose` and is about to lean on
Docker further for sandboxing — adding one more service isn't a new
category of operational burden, just more of an existing one.

## Open questions

- The test suite's shared-cache in-memory SQLite trick
  (`docs/testing.md`) doesn't carry over as-is — needs a real replacement
  (e.g. a per-test schema/database, or a test container) before this can
  be considered done, not left as a regression in test isolation.
