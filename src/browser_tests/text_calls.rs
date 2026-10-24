use super::*;

/// The evidence reply (conversation 2294639360's message 98: prose, then a
/// tool call written as text) as an SSE stream, escaped by `serde_json`.
fn evidence_reply() -> String {
    let text = include_str!("../text_calls/fixtures/evidence_98.txt");
    reply_body(text, "end_turn")
}

/// A one-text-block reply ending with `stop_reason`, as SSE.
fn reply_body(text: &str, stop_reason: &str) -> String {
    use serde_json::json;
    [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 60}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect()
}

/// The transcript's notice lines' text.
async fn notice_lines(page: &chromiumoxide::Page) -> Vec<String> {
    page.evaluate("[...document.querySelectorAll('.system-notice')].map(n => n.innerText)")
        .await
        .expect("read the notices")
        .into_value()
        .expect("a list of strings")
}

/// SME-286: a reply that writes a tool call as text gets smelt's
/// correction, and the turn goes on. The tab shows the model's text, the
/// correction as a notice line naming the call, and the next reply, live
/// and after a reload.
pub(super) async fn scenario_text_call_correction(t: &Scenario<'_>) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let calls = calls.clone();
            async move {
                let body = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => evidence_reply(),
                    _ => reply_body("Done: the comment is saved.", "end_turn"),
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the text-call upstream");
    let address = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let provider = db::create_inference_provider(t.pool, &unique_id("text-call"), "other", &address, "api_key", "key", false, None, true, None)
        .await
        .expect("save the text-call provider");
    let conversation = t.conversation().await;
    db::set_conversation_model(t.pool, conversation.id, provider.id, "qwen")
        .await
        .expect("put the conversation on it");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;

    let turn = crate::turn::run_turn(
        t.pool,
        conversation.id,
        anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: "Post the finished comment.".to_string() }],
        },
    )
    .await;
    let mock = *MOCK_PROVIDER.get().expect("the mock provider");
    db::set_conversation_model(t.pool, conversation.id, mock, MOCK_MODEL)
        .await
        .expect("put the conversation back");
    db::delete_inference_provider(t.pool, provider.id).await.expect("delete the text-call provider");
    turn.expect("a call written as text doesn't fail the turn");

    let expected = "The model wrote a tool call as text (`mcp__linear.app__add_issue_comment`), so it didn't run; smelt told it.";
    for when in ["live", "after a reload"] {
        if when != "live" {
            page.goto(t.url(&format!("conversation/{}", conversation.id))).await.expect("reload the conversation");
            wait_for_live_client(&page, conversation.id).await;
        }
        assert!(
            wait_for_text(&page, "Done: the comment is saved.", Duration::from_secs(10)).await,
            "{when}: the reply after the correction isn't shown"
        );
        assert!(wait_for_text(&page, "Now the finished comment.", Duration::from_secs(10)).await, "{when}: the model's text isn't shown");
        let notices = notice_lines(&page).await;
        assert!(notices.iter().any(|n| n.contains(expected)), "{when}: {notices:?}");
        let raw: usize = page
            .evaluate("[...document.querySelectorAll('.message')].filter(m => m.innerText.includes('Your last reply')).length")
            .await
            .expect("count raw notices")
            .into_value()
            .expect("a number");
        assert_eq!(raw, 0, "{when}: the correction isn't shown as the user's own words");
    }
}
