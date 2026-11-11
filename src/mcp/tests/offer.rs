//! SME-312 (pin): a fresh seat's offer holds only declared tools.
//!
//! The report — an offered `mcp__code__handle` described "placeholder",
//! declared by nothing — has no source in the code (the record is on the
//! ticket: `Connection.tools` has only ever come from a successful
//! `list_all_tools` (0ef51a00), every offered description has carried
//! `labelled_description` since 3f92fdc9 (SME-309), and `routable_offers`
//! (dca3ee66, SME-386 T7) narrows the offer further). This file pins that
//! mechanically at the real seam — `crate::anthropic::tools::tool_definitions`,
//! the call `turn::reply` puts in the provider request — per SME-310's
//! lesson that a seat's self-audit is a report, not evidence.
//!
//! Deliberately not re-pinned here: `routable_offers`' internals
//! (SME-386's tests) and the label's skip edge and purposes (labels.rs,
//! SME-309) — no overlapping coverage.

use super::*;

/// The bug's name shape, declared: a configured server may declare a
/// stub-like tool, and its being offered, labelled, is correct behavior —
/// declared content is the server's. The bug was an entry from *no*
/// declared source. The mock's fixed description also keeps
/// `labelled_description` clear of its skip branch (tools.rs).
const DECLARED_TOOL: &str = "handle";
const DECLARED_DESCRIPTION: &str = "A tool";

/// A fresh root conversation: no parent, no `agents` row — the seat the
/// report describes, so the chain is unruled and every native except the
/// Child pair is offered.
async fn fresh_seat(pool: &sqlx::PgPool) -> i64 {
    crate::db::create_conversation(pool).await.expect("a fresh conversation").id
}

/// Waits, at most `gated_http::WAIT`, for `task` to finish — so a
/// regression fails the test instead of hanging it under the cluster
/// lock (SME-166's rule; registry.rs's helper of the same shape).
async fn finish<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(gated_http::WAIT, task)
        .await
        .expect("the background offer didn't finish in time")
        .expect("the background offer panicked")
}

/// An offer's MCP half — the entries an `mcp__` name marks as
/// server-owned — as (name, description).
fn mcp_half(offer: &[ToolDefinition]) -> Vec<(String, String)> {
    offer
        .iter()
        .filter(|d| d.name.starts_with("mcp__"))
        .map(|d| (d.name.clone(), d.description.clone()))
        .collect()
}

/// The one entry a healthy mock must appear as: its declared tool,
/// namespaced by its own server's name and leading the `[server]` label.
/// The bug's shape — an unlabelled description, or an entry no listed
/// tool backs — fails this equality. The expectation is the literal
/// label, deliberately not `labelled_description`'s own output: an
/// oracle computed by the function under test can't see it mutated away
/// (the plan's bite-check 1 stubs it to a pass-through, and this test
/// must go red). The invariant stays "equals what the composition point
/// composes", not a literal `[server] ` prefix rule — the skip branch
/// (description already leading with the server's own name, tools.rs)
/// is labels.rs's to pin; the mock's description stays clear of that edge.
fn healthy_entry(server: &McpServerConfig) -> (String, String) {
    (
        namespaced_tool_name(&server.name, DECLARED_TOOL),
        format!("[{}] {}", server.name, DECLARED_DESCRIPTION),
    )
}

/// SME-312: a fresh seat's offer holds only declared tools — every entry
/// is a `native_tool_definitions()` name or the configured server's one
/// tool at `namespaced_tool_name(server, "handle")`; the MCP half is
/// exactly that entry, labelled exactly; no name reaches the list twice;
/// and a root seat is offered `notify_user` and not the Child pair. A
/// declared stub-like tool (`handle`) IS offered, labelled — the pin
/// doesn't chase declared content.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_fresh_seats_offer_holds_only_declared_tools(pool: sqlx::PgPool) {
    let healthy = gated_http::spawn(DECLARED_TOOL).await;
    let server = create_static_server(&pool, &healthy.url).await;

    let offer = crate::anthropic::tools::tool_definitions(&pool, fresh_seat(&pool).await).await;

    let names: Vec<&str> = offer.iter().map(|d| d.name.as_str()).collect();
    let expected = healthy_entry(&server);
    let natives: Vec<String> = crate::anthropic::tools::native_tool_definitions()
        .into_iter()
        .map(|d| d.name)
        .collect();
    for name in &names {
        assert!(
            natives.iter().any(|native| native == name) || *name == expected.0.as_str(),
            "{name} is offered with no declared source",
        );
    }
    // The MCP half, exactly: one entry — the bug's name shape carrying
    // its server's label.
    assert_eq!(
        mcp_half(&offer),
        vec![expected],
        "the offer's MCP half isn't the one declared, labelled entry"
    );
    // No name reaches the provider twice (Anthropic refuses such a request).
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        assert!(seen.insert(name), "{name} is offered twice");
    }
    // A fresh root conversation: the TopLevel notice, and not the Child pair.
    assert!(names.contains(&"notify_user"), "a fresh seat isn't offered notify_user");
    for child in ["finish", "ask_parent"] {
        assert!(!names.contains(&child), "a root conversation is offered the agent tool {child}");
    }
    evict(server.id).await;
}

/// SME-312: nothing reaches the offer from a server that never fully
/// connected — `initialize` refused, `tools/list` stalled (held, and
/// re-held: every `tools/list` it gets parks, so "stalls" means a
/// server that never answers), and `tools/list` 500ing after a good
/// `initialize`. Each is attempted (each `initializes()` proves the
/// attempt, so the green is never a vacuous non-attempt) and each
/// leaves nothing behind: not in this offer, and not in one made after
/// the stalled connect gives up — its attempt times out at
/// `CONNECT_TIMEOUT`, `retry_once` reconnects, and the re-held listing
/// times out that too. A half-connected server leaving a stub tool
/// list — the report's hypothesis — has never had a source to grow
/// from, and must not start having one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_offer_carries_nothing_from_a_server_that_never_fully_connects(pool: sqlx::PgPool) {
    let (healthy, refuses, stalls, list_fails) = (
        gated_http::spawn(DECLARED_TOOL).await,
        gated_http::spawn(DECLARED_TOOL).await,
        gated_http::spawn(DECLARED_TOOL).await,
        gated_http::spawn(DECLARED_TOOL).await,
    );
    let (healthy_server, refuses_server, stalls_server, list_fails_server) = (
        create_static_server(&pool, &healthy.url).await,
        create_static_server(&pool, &refuses.url).await,
        create_static_server(&pool, &stalls.url).await,
        create_static_server(&pool, &list_fails.url).await,
    );
    // Arm every failure before the offer that attempts it: `refuses`
    // answers the held `initialize` — and every later request — with a
    // 500; `list_fails` answers `initialize` and 500s the listing;
    // `stalls` answers `initialize` and parks the listing forever.
    refuses.hold("initialize").fail();
    list_fails.hold("tools/list").fail();
    let mut stalled_first = stalls.hold("tools/list");
    let conversation_id = fresh_seat(&pool).await;

    // The first offer runs in the background — a turn's tool list's own
    // shape (registry.rs's `list_tools_in_background` pattern) — so the
    // test can interleave with it: `hold` is one-shot (the handler
    // `take()`s it), so a single arming lets `retry_once`'s second
    // connect ANSWER the listing, and the stalled server would join the
    // offer ~5 ms after the first offer's deadline as a merely-slow
    // server (SME-312's first-run finding; Driver ruling (a)).
    // Re-arming between the attempts makes "stalls" mean never answers.
    let first = tokio::spawn({
        let pool = pool.clone();
        async move { crate::anthropic::tools::tool_definitions(&pool, conversation_id).await }
    });
    stalled_first.reached().await; // attempt 1's listing parks
    let mut stalled_second = stalls.hold("tools/list");
    stalled_second.reached().await; // the retry's listing parks too — nothing can register
    let offer = finish(first).await; // the offer waits only TOOL_LIST_WAIT, lists nothing

    // The MCP half is the healthy entry alone: no name from any of the
    // three, stub or otherwise. Deterministic both ways now: an entry
    // registers only on `Ok`, and neither stalled attempt can reach `Ok`
    // inside the test's window.
    assert_eq!(
        mcp_half(&offer),
        vec![healthy_entry(&healthy_server)],
        "the first offer holds something from a server that never fully connected"
    );
    // Non-vacuity: every failing server really was attempted in this
    // first offer (one `initialize` per connect attempt).
    for (server, mock) in [("refuses", &refuses), ("stalls", &stalls), ("list_fails", &list_fails)] {
        assert!(mock.initializes() >= 1, "{server} was never attempted — the test proved nothing");
    }
    // Past the stalled attempts' give-up (each CONNECT_TIMEOUT, the
    // failure recorded against `RETRY_AFTER_FAILURE`) a second offer
    // still holds nothing: a connect that never completed leaves no
    // late stub. (A merely-slow server that goes on to answer is a
    // different case, and legitimately appears on a later offer —
    // this one never answers.)
    tokio::time::sleep(CONNECT_TIMEOUT * 3 + std::time::Duration::from_secs(1)).await;
    // The stalled server got here twice: `retry_once` reconnected after
    // the first listing timed out, and the re-held listing parked that
    // too (>= 1 each of the others is the first offer's; their retries
    // have run by now as well).
    for (server, mock) in [("refuses", &refuses), ("stalls", &stalls), ("list_fails", &list_fails)] {
        assert!(mock.initializes() >= 2, "{server}'s retry_once attempt never reached the mock");
    }
    let second = crate::anthropic::tools::tool_definitions(&pool, conversation_id).await;
    assert_eq!(
        mcp_half(&second),
        vec![healthy_entry(&healthy_server)],
        "a connect that never completed left an entry in a later offer"
    );
    for id in [healthy_server.id, refuses_server.id, stalls_server.id, list_fails_server.id] {
        evict(id).await;
    }
}
