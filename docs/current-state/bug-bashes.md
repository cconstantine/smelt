# Bug bashes

Part of the [Current state](index.md). What each bug bash swept and fixed; the features themselves are in [features.md](features.md).

* A third bug bash (SME-51, 2026-09-28) ran the Feature checklist and a whole-codebase review, and fixed 12 findings:
  * **Other sites can't act as the user.** `src/request_guard.rs` refuses a write that a browser marks as coming from another site, a sandbox preview included. Once `SMELT_ALLOWED_HOSTS` names the hosts smelt is reached by, requests for any other host name are refused, which stops DNS rebinding.
  * **Conversations are isolated.** Terminal and command tools only act on the calling conversation's ids.
  * **The private-address guard refuses every non-public range,** CGNAT (Tailscale) included, and `http_request` connects to the addresses it checked. (SME-155 has since removed the address filter: the model's fetch tools and browser reach any address.)
  * **A stop is saved in the conversation** as a notice, shown as "Stopped." everywhere and seen by the model and by compaction.
  * **Pods are deleted with a 20-second grace,** and the sandbox agent exits on SIGTERM.
  * **One live pod per conversation** is a unique index, and pod starts are serialised per conversation and survive a Stop.
  * **Tabs recover from a lagging or reconnecting event stream** without missing or doubling anything.

  The rest of the review became SME-52 to SME-63.
* A fourth bug bash (SME-122, 2026-10-09 to 10-15) swept everything since SME-51, ran the Feature checklist on four provider kinds, and fixed 11 findings: a `javascript:` sign-in address from an MCP server's OAuth metadata is refused instead of run in smelt's page; a page load during a pod start no longer stops the starting pod (and a pod stopped mid-start isn't left running); terminals survive output that isn't UTF-8, an `exit`, and a 20 MB line, and lose no stderr; an empty or refused reply no longer breaks later turns, and a refusal shows as a notice; Haiku 4.5 gets a thinking budget instead of adaptive thinking it refuses; a turn's error clears in every tab once a later turn starts; a tab left on a deleted conversation clears; and deleting a conversation keeps its spend. The user declined one finding (B49: the model's `http_request` can reach smelt's own API); the other 38 findings are SME-165 to SME-190, and its stale checklist and docs lines were corrected.
