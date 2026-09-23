# Language server integration

## What

Run a language server (e.g. `rust-analyzer` for a Rust checkout) inside the
sandbox pod and expose its capabilities as model-facing tools —
diagnostics, hover, go-to-definition, find-references, symbol search.
Same idea as opencode's `lsp` tool.

## Why

Right now the model's only way to check whether an edit is even valid is
to run a real build (`cargo check`, etc.) via `run_terminal_command` and
read the output — slow, and coarse (a build error tells you *that*
something's wrong, an LSP's diagnostics tell you exactly where, with the
same squiggly-line precision an IDE gives a human). Go-to-definition and
find-references are capabilities the model has no equivalent for today at
all short of `grep`-ing for a name and hoping it's unambiguous (see
`docs/projects/ideas/glob-and-grep.md`) — a real gap for any nontrivial
refactor.

## Depends on

- A language server actually installed in the sandbox image
  (`docker/sandbox/Dockerfile`) — and, unlike `ripgrep` (one binary, no
  config), a real per-project language server needs the project's own
  toolchain/dependencies already resolved (e.g. `rust-analyzer` wants a
  buildable `Cargo.lock`) before it can answer anything useful. This is a
  meaningfully bigger lift than the other tools on this list.
- A long-lived process per pod, not a one-shot exec like most sandbox
  tools today — closer in shape to how `sandbox_agent.rs` itself is a
  long-lived process than to a `run_terminal_command` invocation. Needs
  its own lifecycle (start once, keep the connection warm across tool
  calls, tear down with the pod).
- A protocol translation layer: LSP itself is JSON-RPC over stdio with a
  specific initialize/handshake/notification shape — `sandbox_agent.rs`
  would need to speak that to the language server process and re-expose
  a simplified request/response shape as tool calls, similar in spirit to
  how it already multiplexes terminal WebSocket traffic.

## Open questions

- Which language(s) to support first — smelt itself is 100% Rust, so
  `rust-analyzer` is the obvious first target, but the sandbox is meant to
  be general-purpose (see `coding-session.md`), so a single hardcoded
  language server may not generalize.
- Does the sandbox image bundle one specific language server, or does
  `create_pod`/the sandbox volume mechanism need a way to select/install
  one per project? Affects image size and how "generic" the sandbox stays.
- This is a much bigger scope than the other tool ideas here — likely
  worth its own plan rather than folding into a batch of smaller tool
  additions.
