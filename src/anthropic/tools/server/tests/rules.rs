//! SME-241: a stored rule chain decides what a conversation is offered
//! and what it may call. Chains here are set by direct SQL — the column
//! arrives with the grammar commit; `start_agent` starts writing it in
//! the next step, so these tests own the evaluation itself.

use super::*;

/// A conversation with a parent and an `agents` row storing `chain`
/// (JSON set directly — a real start assembles the same shape).
async fn child_with_chain(pool: &PgPool, chain: Option<Value>) -> i64 {
    let parent = db::create_conversation(pool).await.expect("parent");
    let child = db::create_conversation(pool).await.expect("child");
    sqlx::query("UPDATE conversations SET parent_id = $1 WHERE id = $2")
        .bind(parent.id)
        .bind(child.id)
        .execute(pool)
        .await
        .expect("parent link");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, rule_chain)
         VALUES ($1, $2, 1, 'a brief', 'active', $3)",
    )
    .bind(child.id)
    .bind(parent.id)
    .bind(chain)
    .execute(pool)
    .await
    .expect("agents row");
    child.id
}

fn chain_of(entries: Vec<Value>) -> Option<Value> {
    Some(Value::Array(entries))
}

fn entry(class: &str, allow: &[&str], deny: &[&str]) -> Value {
    serde_json::json!({ "class": class, "allow": allow, "deny": deny })
}

async fn offered_names(pool: &PgPool, conversation: i64) -> Vec<String> {
    let mut names: Vec<String> =
        tool_definitions(pool, conversation).await.into_iter().map(|def| def.name).collect();
    names.sort();
    names
}

fn missing<'w>(names: &[String], want: &[&'w str]) -> Vec<&'w str> {
    want.iter().copied().filter(|n| !names.iter().any(|have| have == *n)).collect()
}

fn present<'w>(names: &[String], unwanted: &[&'w str]) -> Vec<&'w str> {
    unwanted.iter().copied().filter(|n| names.iter().any(|have| have == *n)).collect()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_read_only_chain_hides_writing_tools_from_the_offer(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("read-only", &[], &[])])).await;
    let names = offered_names(&pool, child).await;
    assert_eq!(
        missing(
            &names,
            &["read_file", "todoread", "http_request", "browser_read", "list_agents", "create_pod"],
        ),
        Vec::<&str>::new(),
        "a read-only ceiling keeps its own class offered: http_request by its read-only side, \
         create_pod by its own pin (class/tests.rs)"
    );
    assert_eq!(
        present(&names, &["browser_click", "browser_fill", "stop_agent"]),
        Vec::<&str>::new(),
        "the writing tools go"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_chain_offers_an_agent_what_untouched_conversations_see(pool: PgPool) {
    let child = child_with_chain(&pool, None).await;
    let names = offered_names(&pool, child).await;
    assert_eq!(
        missing(&names, &["read_file", "browser_click", "http_request", "stop_agent"]),
        Vec::<&str>::new(),
        "no entries means allow-all, as before the column existed"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deny_pattern_hides_matching_tools_and_keeps_the_rest(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("all", &[], &["browser_*"])])).await;
    let names = offered_names(&pool, child).await;
    assert_eq!(
        present(&names, &["browser_click", "browser_fill", "browser_read", "browser_navigate", "browser_back"]),
        Vec::<&str>::new(),
        "a deny names tools, not classes: every browser_* goes, read-only ones too"
    );
    assert_eq!(
        missing(&names, &["read_file", "close_browser_session", "webfetch"]),
        Vec::<&str>::new(),
        "the rest is untouched — close_browser_session is no browser_*"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_exact_allow_beats_a_wildcard_deny_at_the_offer(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("all", &["browser_read"], &["bro*"])])).await;
    let names = offered_names(&pool, child).await;
    assert!(names.iter().any(|n| n == "browser_read"), "level 1 outranks level 2: {names:?}");
    assert_eq!(present(&names, &["browser_click"]), Vec::<&str>::new());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deny_wins_a_tie_at_one_level(pool: PgPool) {
    // Both patterns are wildcards (level 2) and both match browser_read.
    let child = child_with_chain(&pool, chain_of(vec![entry("all", &["bro*"], &["browser_*"])])).await;
    let names = offered_names(&pool, child).await;
    assert_eq!(present(&names, &["browser_read", "browser_click"]), Vec::<&str>::new());
    assert!(names.iter().any(|n| n == "webfetch"), "the rest is untouched");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_chain_entry_can_be_undercut_by_another(pool: PgPool) {
    // The chain is a conjunction: every entry must allow, so an entry's
    // allow can't undo an earlier entry's ceiling, and order is moot.
    for entries in [
        vec![entry("read-only", &[], &[]), entry("all", &["stop_agent"], &[])],
        vec![entry("all", &["stop_agent"], &[]), entry("read-only", &[], &[])],
    ] {
        let child = child_with_chain(&pool, chain_of(entries)).await;
        let names = offered_names(&pool, child).await;
        assert!(!names.iter().any(|n| n == "stop_agent"), "a later allow can't lift an earlier ceiling: {names:?}");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_writing_call_is_refused_naming_the_ceiling_and_records_its_class(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("read-only", &[], &[])])).await;
    let (result, spans) =
        crate::telemetry::capture_spans(execute(&pool, child, "t1", "stop_agent", &serde_json::json!({}))).await;
    let message = result.expect_err("a writing call under a read-only ceiling is refused");
    assert!(
        message.contains("stop_agent is denied by this conversation's tool rules")
            && message.contains("class ceiling read-only doesn't allow a writing tool"),
        "{message}"
    );
    let span = spans.iter().find(|span| span.name == "tool stop_agent").expect("a tool span");
    assert_eq!(span_attribute(span, "smelt.tool.class"), Some(&"writing".into()), "the refused call records its class");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_read_only_ceiling_offers_http_request_and_refuses_its_reaching_calls(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("read-only", &[], &[])])).await;
    let names = offered_names(&pool, child).await;
    assert!(names.iter().any(|n| n == "http_request"), "offered by the read-only side of its class range");

    // The tool's own input error, not the rule's, proves a GET-class call
    // got through: {} has no method, which the class function reads as GET.
    let message = execute(&pool, child, "t1", "http_request", &serde_json::json!({})).await.expect_err("its own error");
    assert!(message.contains("missing or non-string field: url"), "not a rules refusal: {message}");

    for (method, class) in [("POST", "writing"), ("DELETE", "destructive"), ("nonsense", "writing")] {
        let message = execute(
            &pool,
            child,
            "t2",
            "http_request",
            &serde_json::json!({"url": "http://127.0.0.1:1/", "method": method}),
        )
        .await
        .expect_err("refused before any request");
        assert!(
            message.contains("http_request is denied by this conversation's tool rules")
                && message.contains(&format!("doesn't allow a {class} tool")),
            "{method}: {message}"
        );
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_denied_tool_call_names_the_deny_pattern(pool: PgPool) {
    let child = child_with_chain(&pool, chain_of(vec![entry("all", &[], &["todoread"])])).await;
    let message = execute(&pool, child, "t1", "todoread", &serde_json::json!({})).await.expect_err("denied by pattern");
    assert!(
        message.contains("todoread is denied by this conversation's tool rules (deny pattern `todoread`)"),
        "{message}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_corrupt_chain_fails_open_consistently_in_offer_and_call(pool: PgPool) {
    // The column is smelt-written JSON; one that won't read as rule entries
    // (hand-corrupted, say) falls open with a warning — offer and call must
    // agree, or a conversation is offered tools whose calls are refused.
    let child = child_with_chain(&pool, Some(Value::String("not-json".to_string()))).await;
    let names = offered_names(&pool, child).await;
    assert!(names.iter().any(|n| n == "browser_click"), "offer falls open");
    let message = execute(&pool, child, "t1", "http_request", &serde_json::json!({})).await.expect_err("its own error");
    assert!(message.contains("missing or non-string field: url"), "the call falls open the same way: {message}");
}
