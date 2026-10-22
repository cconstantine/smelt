# Tool rules, classes, credentials and egress
Part of the [personas design](index.md) (SME-234).

Tool rules are about effects outside the agent's own sandbox (the project's scope item 5): what an agent does in its own pod is its business; what reaches out is limited. This page gives each tool a class, the grammar a persona uses to allow and deny, how a child's rules are narrowed by its parent's, and the two things tool rules can't cover on their own because bash reaches the network: credentials and egress.

## Classes

Every tool has one of three classes, in this order:

| Class | Means |
| -- | -- |
| `read-only` | Changes nothing outside the agent's own sandbox and conversation. Work in its own pod, on its own `/workspace`, on a volume mounted in its pod, and on its own conversation's state (todos, a question to the user) counts as read-only. |
| `writing` | Changes something outside, additively or reversibly: a PR review, a new conversation, a message to another agent, a form submitted on a website. |
| `destructive` | Can delete or overwrite something outside: merging, deleting a volume or a provider, an HTTP `DELETE`. |

These are the MCP spec's own distinctions (`readOnlyHint`; `destructiveHint`, "meaningful only when `readOnlyHint == false`", `rmcp` 3.1.2's `ToolAnnotations`), so one scale covers smelt's tools and MCP tools.

### smelt's own tools

Each native tool's class sits beside its definition in the registry (`Tool { def, run, class }`, `src/anthropic/tools/mod.rs`), and a test fails when one has none (SME-237).

| Tools | Class | Why |
| -- | -- | -- |
| `create_pod`, `terminate_pod`, `list_pods`, `create_terminal`, `terminate_terminal`, `list_terminals`, `run_terminal_command`, `send_signal`, `terminal_command_status`, `read_terminal_output`, `list_commands`, `set_idle_notice` | read-only | The agent's own pod. A command can reach the network, which is what [credentials](#credentials) and [egress](#egress) are for, not the tool's class. |
| `read_file`, `edit_file`, `write_file`, `list_directory`, `glob`, `grep` | read-only | Files in its own pod, a mounted volume included. |
| `todowrite`, `todoread` | read-only | The conversation's own list. |
| `clone_repo` | read-only | Fetches into its own `/workspace`. Pushing needs the credential, not this tool. |
| `load_instructions`, `list_personas` | read-only | Reads its own checkout; may ask the user to trust a repo, which is the user's decision. |
| `webfetch`, `sandbox_preview_url` | read-only | A `GET` in a browser, or a link to its own pod's server. |
| `http_request` | by its `method`, parsed by the same function that sends it (`parse_method`, `src/http_request.rs`, which uppercases first, so `delete` is `DELETE`; absent is `GET`): `GET`, `HEAD`, `OPTIONS` read-only; `DELETE` destructive; any other writing | The one native tool whose class depends on its input; see [Offered and called](#offered-and-called). |
| `open_browser_session`, `close_browser_session`, `browser_navigate`, `browser_back`, `browser_read` | read-only | Reading pages. |
| `browser_click`, `browser_fill` | writing | Can submit a form on any site. A persona that only drives its own preview allows them by name. |
| `lsp_servers`, `start_language_server`, `lsp` | read-only | A language server for its own checkout. |
| `ask_user` | read-only | Asks the user. |
| `list_agents` | read-only | Its own children. |
| `message_agent`, `ask_parent`, `finish` | read-only | They reach only the agent's own parent or children, which the parent chose to start; a read-only Code Reviewer must still be able to report. ([agents.md](agents.md#tools)) |
| `start_agent`, `stop_agent` | writing | New conversations, pods and spend; ending another agent's work. |
| `create_volume` | writing | A claim outside the pod ([checkouts-and-volumes.md](checkouts-and-volumes.md#the-volume-tools)). |
| `delete_volume` | destructive | Deletes a claim other agents may mount. |

`message_agent`, `ask_parent`, `finish`, `list_personas`, the agent and volume tools are added by SME-235, 236, 238 and 239; SME-236's land without a class, and SME-237 classes them with the rest (the user's order, 2026-10-22).

### MCP tools

From the server's annotations, which `mcp_tool_to_definition` (`src/mcp/tools.rs`) drops today:

| `readOnlyHint` | `destructiveHint` | Class |
| -- | -- | -- |
| `true` | any | read-only |
| `false` or absent | `false` | writing |
| `false` or absent | `true` or absent | destructive |

A tool with no annotations is destructive, the spec's defaults (`readOnlyHint` false, `destructiveHint` true). Many servers don't annotate, so the user's **override** (SME-240) matters: a table `mcp_tool_classes (server_id → mcp_servers ON DELETE CASCADE, tool_name, class, PRIMARY KEY (server_id, tool_name))`. It keys on the server's id, so renaming a server keeps its overrides; an override for a tool the server no longer lists stays and is shown as such. The class of an MCP tool is the override if any, else the annotations.

## The grammar

A persona's rules are its file's `smelt.tools` ([persona-files.md](persona-files.md#the-format-markdown-with-yaml-frontmatter-smelts-settings-under-one-key)):

```yaml
smelt:
  tools:
    class: read-only            # all (default), no-destructive or read-only
    allow: [github/create_pull_request_review, linear/save_comment]
    deny: [browser_*]
```

- **`class`** is the ceiling: `all` allows every class, `no-destructive` read-only and writing, `read-only` read-only. The user's two named classes are `read-only` and `no-destructive`.
- **`allow` and `deny`** are lists of tool patterns. A smelt tool is named as the model sees it (`write_file`). An MCP tool is `<server>/<tool>`, by the server's configured name (`mcp_servers.name`) and the tool's own name: `github/merge_pull_request`, not `mcp__github__merge_pull_request`. `*` matches any run of characters other than `/`, in a tool's name only: a pattern's server is always named (`*/get_*` is an error), so a server rule can't be undercut by a pattern that names no server. `<server>/*` is the whole server.
- An unknown class, an empty pattern, a pattern with more than one `/`, or a name in both `allow` and `deny` is an error naming the file. A server that isn't configured in this deployment is not an error (another deployment may have it); `list_personas` and `start_agent` report the rules that match no configured server, so a typo shows.

**How specific a pattern is**, most specific first:

| Level | Pattern | Example |
| -- | -- | -- |
| 1 | an exact tool name | `github/create_pull_request_review`, `write_file` |
| 2 | a name with a wildcard in it | `github/get_*`, `browser_*` |
| 3 | a whole server | `github/*` |
| 4 | the class ceiling | `class: read-only` |

**Evaluation** for a tool: take the most specific level that has a pattern matching it, in `allow` or `deny`. If both lists match at that level, **deny wins**. If no pattern matches, the class decides: allowed when the tool's class is within the ceiling. A server is more specific than a class because naming a server is a decision about that server, where a class is a decision about every tool at once ("no GitHub at all" beats "anything read-only").

**Worked example,** "read-only, except `github/create_pull_request_review`": `class: read-only`, `allow: [github/create_pull_request_review]`.

| Tool | Its class | Decided at | Result |
| -- | -- | -- | -- |
| `github/create_pull_request_review` (say it's unannotated) | destructive | level 1 allow | allowed |
| `github/get_pull_request` (`readOnlyHint: true`) | read-only | level 4 | allowed |
| `github/merge_pull_request` | destructive | level 4 | denied |
| `write_file` | read-only | level 4 | allowed |
| `start_agent` | writing | level 4 | denied |

**Edge cases:**

- *A deny and an allow at one level:* `allow: [github/*]`, `deny: [github/*]` is the same name in both lists: an error. `allow: [github/get_*]`, `deny: [github/*_pull_*]` both match `github/get_pull_request` at level 2: denied.
- *A wildcard against an exact name:* `deny: [github/*]`, `allow: [github/get_pull_request]`: `get_pull_request` is allowed (level 1), every other GitHub tool denied (level 3).
- *A server renamed:* rules name servers, so after the user renames `github` to `gh`, `github/...` patterns match nothing and those tools fall to the class ceiling. Never wider than the ceiling, and the listing reports the stale patterns.
- *A server name with `/` or `*` in it:* today nothing stops one (`mcp_servers.name` is only `UNIQUE`). SME-241 refuses `/` and `*` in new server names and reports existing ones; their tools can be ruled on only by class.
- *A conversation without a persona and without a parent* (the user's own): every tool, as today.

### Offered and called

One function decides both (SME-241): a tool is **offered** (in the request's `tools`, `tool_definitions` built per conversation) when the rules allow it at some class it can have, and a **call** is refused, with a tool error saying which rule, when they don't allow it at the class of that call. Only `http_request` has more than one class, so a read-only persona is offered it and its `POST` is refused. The alternative, splitting `http_request` into a read tool and a write tool, changes what every conversation is offered for one persona's sake.

The list a conversation is offered changes only when its rules or the configured MCP servers change. A persona's rules are fixed when its agent starts ([narrowing](#narrowing-a-child-never-gets-more-than-its-parent)), so prompt caching keeps working within a conversation.

## Narrowing: a child never gets more than its parent

A child's **effective rules** are its persona's rules (allow-all when it has no persona) **and** its parent's effective rules: a tool is allowed for the child only if both allow it. The user's own conversation, at the top, has allow-all. So for a chain root → A → B → C, C may use a tool only if C's persona, B's, A's and root's rules all allow it; each step is a conjunction, and a conjunction can't allow anything one of its parts denies, so no descendant can be wider than any ancestor.

At `start_agent`, smelt stores the child's **rule chain**: the parent's stored chain plus the child's own persona rules, as data (JSONB on the child's `agents` row, [agents.md](agents.md#the-schema)). Evaluation walks the chain (as deep as the nesting limit), so nothing is recomputed from files that may have changed. The same goes for credentials and volumes: a child gets a credential only if its persona lists it and its parent has it, and only volumes its parent created or has mounted.

**Alternatives:** computing the child's rules once as a flat allow-set loses wildcards and the class ceiling, so a tool from an MCP server added later would be judged by nobody; inheriting only the persona's rules (opencode, whose subagent uses its own agent's permissions, `packages/core/src/tool/plugin/subagent.ts`) lets a child of a read-only parent write.

## Credentials

What a sandbox pod holds today: the one SSH key and the commit identity, written into `/etc/smelt` by `install_git_files` (`pod_git_files`, `src/git/server/keys.rs`) at pod start (`install_into_new_pod`) and into every live pod when the key changes (`install_into_live_pods`). Nothing else: the pod spec's only environment variable is `WORKSPACE_OWNER` (`src/sandbox/spec.rs`). The namespace's default service-account token is mounted, since the spec doesn't set `automountServiceAccountToken: false`; that's SME-63's.

**Design (SME-242):** a persona lists the credentials it gets, `smelt.credentials: [git-ssh]`. A persona with no `credentials` field gets none; a child without a persona gets its parent's; narrowing applies. One function, `pod_credentials(conversation)`, decides, and all three install sites ask it: pod start, the live-pod install after a key change, and a pod restart.

A child without `git-ssh` still needs its repo: smelt installs the key, runs the clone, and removes `/etc/smelt/keys/*` and the `IdentityFile` lines before the child's first turn, so no command of the model's ever runs while the key is there: the child is `starting` until then, no turn of any kind is admitted while it is, and the key is removed on every way out of `starting`, a failed or cut-off start included ([agents.md](agents.md#statuses)). Its later `git fetch` over SSH fails; its environment section says it has no git credential and why, so the model doesn't loop on it. A read-only deploy key would let it fetch, but smelt stores one key (`20260927130000_one_ssh_key.sql`, `refuse_a_second_key`); that's a later choice.

**The Project Manager needs one more credential:** `scripts/pm-wait-for-board` reads the user's Linear API key from `.linear-api-key`. Under smelt, a Project Manager's pod has no such file. So SME-242 generalizes `credentials` to **named secrets the user keeps in smelt** (a settings page; each a name and a value, written to `/etc/smelt/secrets/<name>` mode 600 by the same install path), with `git-ssh` the built-in one. That adds scope to SME-242 (SME-234, question 4).

## Egress

Bash reaches the network, so tool rules alone don't stop an agent from `curl`ing around them. Two things matter, and the second is the more serious:

1. **The outside world:** without credentials, bash can't write to GitHub or Linear as the user, so for those the credential rule above is the guard.
2. **smelt itself.** smelt has no login; `request_guard` refuses cross-site browser writes, and "a request with neither header (curl, a script) is let through" (`src/request_guard.rs`). A sandbox pod that can reach smelt's address can call any server function with `curl`: start turns in other conversations, change providers, import keys. That walks around every tool rule. Today every pod can reach smelt (its public URL at least), the user's own conversations' included.

**Spike S3** (a throwaway k3s `v1.34.6-k3s1`, the version compose and terraform pin, in this host's Docker; never the shared clusters): a pod shaped like a sandbox (a privileged native Docker sidecar running `docker:29-dind`, and a main container) under an egress NetworkPolicy allowing only cluster DNS and `1.1.1.1:443`:

| Check | No policy | With the policy |
| -- | -- | -- |
| main container to `example.com:443` | 200 | refused |
| main container to `1.1.1.1:443` (allowed) | 301 | 301 |
| DNS from the main container | resolves | resolves (DNS allowed by the policy) |
| the Docker sidecar to `example.com` | ok | blocked |
| a container the sidecar runs, to `example.com` | ok | blocked |
| a container the sidecar runs, to `1.1.1.1:443` (allowed) | ok | open |
| a container the sidecar runs, to `1.0.0.1:443` | not tried | closed |
| `docker pull` in the sidecar | ok | blocked |
| `kubectl exec` into the pod | ok | ok |
| `kubectl port-forward` into the pod | ok | ok |

So on a stock k3s, NetworkPolicy (k3s's embedded network-policy controller, on unless `--disable-network-policy`) holds for the whole pod, the Docker sidecar and its containers included (their traffic leaves through the pod's own interface), and smelt's own routes into the pod (exec for commands and file writes, port-forward for the agent's WebSocket and previews) keep working, since they come in, not out. Spike S2 ran on the same cluster ([checkouts-and-volumes.md](checkouts-and-volumes.md#sharing-a-claim-between-pods)).

**What a rule can say:** NetworkPolicy matches addresses (`ipBlock` CIDRs, with `except`), pods and namespaces, never host names. So:

- `egress: open` (the default): as today.
- `egress: none`: nothing out, cluster DNS included. `docker pull`, `cargo fetch` and `npm install` fail; the persona works with what's in its checkout and on its volumes.
- `egress: {allow: [<CIDR>…]}`: those, plus cluster DNS.
- **Never smelt.** Whatever a persona says, a pod running a persona with any tool rule narrower than allow-all can't reach smelt's own addresses: a deployment setting (`SANDBOX_SMELT_CIDRS`: smelt's pod or service range and the address its public name resolves to) added to every such pod's policy as `except`. Without it, the tool rules are advisory.
- **smelt's own fetchers too.** `http_request`, `webfetch` and the browser tools run in smelt's own process, not in the pod (`src/http_request.rs`, `src/webfetch.rs`, `src/browsing.rs`), so no pod policy covers them, and since SME-155 they check no address (`src/fetch_guard.rs`). For a conversation under narrowed rules they refuse smelt's own addresses (loopback, `SANDBOX_SMELT_CIDRS`, the addresses `SMELT_BASE_URL`'s host resolves to) and follow its persona's `egress`, checked on the address they resolve and connect to (`http_request` already resolves once per hop; the browser's requests go through `egress_proxy`, which resolves too). The sandbox's own `localhost` route stays open: that's its own pod. Otherwise a `writing` `http_request` `POST` to `http://127.0.0.1:<port>/api/…` answers trust cards or imports keys, and a read-only `GET` carries data past `egress: none`.
- **Host names** (`github.com` only) would need a forward proxy that checks the TLS SNI or `Host` and pods forced through it (`HTTPS_PROXY` plus a policy allowing only the proxy). smelt's `egress_proxy` is the other direction (smelt's browser into a pod) and isn't one. Out of scope here; a ticket if a persona needs it.

**What SME-243 must do first:** check whether the homelab cluster enforces NetworkPolicy. The `park` role can't read k3s's flags; the user can (`k3s` started without `--disable-network-policy`, and kube-router's `KUBE-ROUTER-*` chains in `iptables-save` on the node), or SME-243's real-cluster test checks it directly. The `park` role needs `networkpolicies` (`networking.k8s.io`: `create`, `get`, `list`, `delete`) in `k8s/smelt-park-rbac.yaml` and `terraform/smelt/rbac.tf`, and on homelab by hand. Each policy selects one pod by a label of its own, carries `smelt/instance`, and goes with the pod, like claims (SME-115).

**If homelab doesn't enforce it,** SME-243 shrinks to the setting that makes it enforce (or cancels, with the user), and the credential rule is the only bash-side limit; the "never smelt" gap stays open and is said so in the docs and to the user.
