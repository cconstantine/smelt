# glob and grep sandbox tools

**Branch:** `glob-and-grep` · **Idea:** `projects/ideas/glob-and-grep.md` (removed) · **Plan:** `projects/plans/glob-and-grep.md` (removed)

## What shipped

Two new model-facing tools, `glob` and `grep`, giving the model structured
file discovery and content search inside its conversation's sandbox pod —
the same request/response shape every existing file tool
(`read_file`/`write_file`/`edit_file`/`list_directory`) already uses,
rather than a `run_terminal_command` call the model has to hand-write and
parse text output from.

- **`glob`** finds files under a given root whose path relative to that
  root matches a glob pattern (`**/*.rs` for every `.rs` file at any depth,
  `*.rs` for only ones directly in the root — a bare `*` never crosses a
  directory separator, only `**` does).
- **`grep`** searches file contents under a root for a regex pattern,
  optionally narrowed to files matching a glob filter first, returning
  each match's file, 1-indexed line number, and line text. Every file it
  excludes (not valid UTF-8, or over a 5 MiB size cap) is named, with why,
  in the response's `skipped` list — never silently dropped.
- Both are paginated (`offset`/`limit`, low defaults — 50/200 for `glob`,
  20/100 for `grep`) with a separate, higher hard scan ceiling
  (`MAX_GLOB_SCAN`/`MAX_GREP_SCAN` in `sandbox_agent.rs`) bounding
  worst-case walk cost independent of the page actually requested —
  `scan_capped` in the response is true only when *that* ceiling was hit,
  distinct from ordinary "more pages exist."
- Implemented in-process (the `ignore` and `regex` crates — the same
  library ripgrep itself is built on — plus `globset` for pure
  single-pattern matching), not by shelling out to an installed `rg`
  binary: results come back structured rather than parsed from a
  subprocess's stdout, the matching/pagination logic is directly
  unit-tested, and no new OS package was needed in
  `docker/sandbox/Dockerfile`.

**Verification:** 216 `cargo test --features server` tests passing (up
from 206 at the start of this session — see the retrospective's first
point), both build targets clean, `cargo fmt --check` clean, and the real
end-to-end round trip (recursive vs. non-recursive glob matching,
cross-file grep, grep's glob filter) verified against a real pod in
`test_terminal_lifecycle_end_to_end`.

## Retrospective

**What worked:**
- **Verifying external crate APIs against source, not memory, caught two
  real issues before they shipped.** The plan proposed reusing
  `ignore::overrides::Override` for pattern matching to avoid a second
  crate; reading its actual source showed it's a root-anchored
  whitelist/gitignore-precedence matcher built for filtering a walk, not a
  plain "does this path match this one pattern" check — `globset::Glob`
  was the right fit instead (already a transitive dependency, so no new
  footprint). Then, writing the test *first* for that pattern-matching
  function caught a second, subtler issue: `globset`'s own default lets a
  bare `*` cross a `/`, which would have made `*.rs` and `**/*.rs` behave
  identically — silently defeating the whole reason the tool's own schema
  documents `**` as the recursive marker. `GlobBuilder::new(pattern)
  .literal_separator(true)` fixed it; the red test run is what surfaced
  the gap, not code review after the fact.
- **Splitting pure logic from I/O glue let almost the entire feature get
  strict TDD (red shown before green) — pagination, pattern compilation,
  and grep's line-matching are all unit-tested directly — while the walk
  itself (real filesystem I/O, `ignore::WalkBuilder` under
  `spawn_blocking`) followed the codebase's own established precedent:
  mirrored `handle_list_directory`'s shape, which itself has no direct
  unit test, verified only by the real-cluster integration test instead.
  Flagged explicitly as a mechanical-mirror exception rather than silently
  skipping TDD for it.
- **Running the real integration test against an actually-built sandbox
  image caught a real bug — in the test's own fixture, not the product.**
  `notes.txt`'s content, `"just text, no fn here"`, was written to prove
  `grep`'s glob filter excludes non-`.rs` files — except it accidentally
  contains the literal substring `"fn "` ("...no **fn h**ere"), so `grep`
  correctly matched it, correctly breaking the test's own assertion. Fixed
  by rewording the fixture. A test that had stayed unexecuted (as it did
  for most of this session — see below) would have shipped this
  self-defeating assertion undetected.

**What caused friction, surprise, or rework:**
- **The 10 real-cluster tests failing at the start of this session
  weren't a fundamental environment limitation — `scripts/build-sandbox-
  image.sh` had simply never been run here.** This environment has a
  fully functional DinD sidecar and k3s cluster; running the one manual
  step documented in `setup.md` (build the image, stream it into the
  cluster's node, no registry involved) immediately fixed 9 of the 10
  failures with zero code changes, before the 10th (the newly-extended
  integration test) even got a chance to run for real and catch the
  fixture bug above. Worth checking for a missing setup step before
  concluding a fresh environment "can't" run a given test tier — the same
  "stop iterating on the same theory, get real observability" spirit
  `development-process.md` already calls out for CI investigations
  applies here too, just for an environment-setup symptom rather than a
  code bug.
- **A doc-comment insertion mistake slipped past every compiler/test check
  silently.** Adding `compile_glob_pattern` just above `paginate_slice`
  left `paginate_slice`'s own preceding doc comment stranded above the new
  function instead, leaving `paginate_slice` itself undocumented — `cargo
  check`/`cargo test` have no way to catch a misplaced doc comment, since
  it's not a compile error. Only caught by a deliberate close read of the
  inserted region afterward. Worth doing that close read as a matter of
  course after several small sequential insertions into the same block,
  not just trusting green tests.

**What to change:**
- No process changes proposed this time — the two "what worked" findings
  above (verify-against-source, split-pure-from-glue) are already
  documented rules/patterns in `development-process.md`; this project is
  another confirming data point for them, not a new rule.
