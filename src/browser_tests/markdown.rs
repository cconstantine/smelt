use super::*;

/// SME-30: the model's replies render as markdown, and the reply is
/// untrusted. A heading, list, table, code block and link become those
/// elements; raw HTML (an `<img onerror>`, a `<script>`) and a
/// `javascript:` link stay text and never run; the user's own message
/// stays plain; a code block's copy button copies its code.
pub(super) async fn scenario_markdown_reply(t: &Scenario<'_>) {
    let conversation = t.conversation().await;
    db::create_message(
        t.pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "Show **markdown** please".into() }],
    )
    .await
    .expect("seed the user's message");
    let reply = "## Plan\n\n- first\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() { let x = 1; }\n```\n\n\
        See [the docs](https://example.com/docs) or [this](javascript:window.__smeltPwned=2).\n\n\
        Inline <img src=x onerror=\"window.__smeltPwned=1\"> html.\n\n<script>window.__smeltPwned=3</script>\n";
    db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: reply.into() }])
        .await
        .expect("seed the reply");
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    wait_for_element(&page, ".message-assistant .markdown h2", Duration::from_secs(10)).await;
    let facts: serde_json::Value = page
        .evaluate(
            "(() => { const md = document.querySelector('.message-assistant .markdown');               const link = md.querySelector('a');               return {                 heading: md.querySelector('h2')?.innerText,                 items: md.querySelectorAll('ul > li').length,                 checked: md.querySelector('li.md-task input[type=checkbox]')?.checked ?? null,                 cells: Array.from(md.querySelectorAll('td')).map(c => c.innerText),                 highlighted: md.querySelectorAll('pre.hl-code .hl-keyword, pre.hl-code .hl-storage').length,                 lang: md.querySelector('.md-code-lang')?.innerText,                 links: Array.from(md.querySelectorAll('a')).map(a => a.getAttribute('href')),                 target: link?.getAttribute('target'), rel: link?.getAttribute('rel'),                 imgs: md.querySelectorAll('img').length, scripts: md.querySelectorAll('script').length,                 text: md.innerText, pwned: window.__smeltPwned ?? null,                 user: document.querySelector('.message-user .message-text')?.innerText,                 userStrong: document.querySelectorAll('.message-user strong').length }; })()",
        )
        .await
        .expect("read the rendered reply")
        .into_value()
        .expect("facts");
    assert_eq!(facts["heading"], "Plan", "{facts}");
    assert_eq!(facts["items"], 2, "{facts}");
    assert_eq!(facts["checked"], true, "a checked task item: {facts}");
    assert_eq!(facts["cells"], serde_json::json!(["1", "2"]), "{facts}");
    assert!(facts["highlighted"].as_i64().unwrap_or(0) > 0, "the rust block should be highlighted: {facts}");
    assert_eq!(facts["lang"], "rust", "{facts}");
    assert_eq!(facts["links"], serde_json::json!(["https://example.com/docs"]), "only the https link is a link: {facts}");
    assert_eq!(facts["target"], "_blank", "{facts}");
    assert_eq!(facts["rel"], "noopener noreferrer", "{facts}");
    assert_eq!(facts["imgs"], 0, "raw HTML mustn't become an image: {facts}");
    assert_eq!(facts["scripts"], 0, "{facts}");
    assert!(facts["pwned"].is_null(), "the reply's raw HTML ran: {facts}");
    let text = facts["text"].as_str().unwrap_or_default();
    assert!(text.contains("<img src=x onerror=") && text.contains("<script>"), "raw HTML should show as text: {text}");
    assert_eq!(facts["user"], "Show **markdown** please", "the user's message stays plain: {facts}");
    assert_eq!(facts["userStrong"], 0, "{facts}");

    // The copy button hands the block's code to the clipboard.
    page.evaluate("navigator.clipboard.writeText = async text => { window.__smeltCopied = text; }")
        .await
        .expect("watch the clipboard");
    click_when_present(&page, ".md-code-copy", Duration::from_secs(5)).await;
    assert!(wait_for_text(&page, "Copied", Duration::from_secs(5)).await, "the button should say it copied");
    let copied: Option<String> = page
        .evaluate("window.__smeltCopied ?? null")
        .await
        .expect("read what was copied")
        .into_value()
        .expect("a string");
    assert_eq!(copied.as_deref(), Some("fn main() { let x = 1; }\n"));
}

/// SME-30: the streaming reply renders as markdown as it arrives. A code
/// fence that hasn't closed yet shows plain and is highlighted once it
/// closes, and the reply looks the same once it's saved.
pub(super) async fn scenario_markdown_streaming(t: &Scenario<'_>) {
    use crate::events::{ConversationEvent, publish};
    let conversation = t.conversation().await;
    let page = t.tab(t.url(&format!("conversation/{}", conversation.id))).await;
    wait_for_live_client(&page, conversation.id).await;
    publish(conversation.id, ConversationEvent::ReplyReset {});
    let first = "Here is **bold** and code:\n\n```rust\nfn main() {\n";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: first.into(), offset: 0 });
    wait_for_element(&page, ".message-streaming .markdown strong", Duration::from_secs(10)).await;
    let open_fence: serde_json::Value = page
        .evaluate(
            "(() => { const pre = document.querySelector('.message-streaming .markdown pre');               return { pre: !!pre, highlighted: pre?.classList.contains('hl-code') ?? null, code: pre?.innerText }; })()",
        )
        .await
        .expect("read the open fence")
        .into_value()
        .expect("facts");
    assert_eq!(open_fence["pre"], true, "the open fence should already be a code block: {open_fence}");
    assert_eq!(open_fence["highlighted"], false, "an open fence shows plain: {open_fence}");
    let rest = "}\n```\n\nDone.";
    publish(conversation.id, ConversationEvent::ReplyDelta { text: rest.into(), offset: first.len() });
    wait_for_element(&page, ".message-streaming .markdown pre.hl-code", Duration::from_secs(10)).await;
    let streamed: String = page
        .evaluate("document.querySelector('.message-streaming .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the streamed reply")
        .into_value()
        .expect("html");
    let full = format!("{first}{rest}");
    let saved = db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: full }])
        .await
        .expect("save the reply");
    publish(conversation.id, ConversationEvent::MessagesAppended { messages: vec![saved] });
    assert!(
        wait_for_count(&page, ".message-streaming", 0, Duration::from_secs(10)).await,
        "the saved reply should replace its streaming copy"
    );
    let saved_html: String = page
        .evaluate("document.querySelector('.message-assistant .markdown').innerHTML.replace(/ data-dioxus-id=\"\\d+\"/g, '')")
        .await
        .expect("read the saved reply")
        .into_value()
        .expect("html");
    // Dioxus's own element ids differ between the two mounts; the reply
    // itself mustn't.
    assert_eq!(saved_html, streamed, "the reply changed when it was saved");
}

/// The plain-HTTP name the tier's Chrome resolves to this machine
/// (`--host-resolver-rules`), so a tab can open the app from an origin
/// that isn't a secure context, as on a LAN address (SME-105).
pub(super) const INSECURE_HOST: &str = "smelt-http.test";

/// Facts about each Copy reply button on a page: the reply its bubble
/// belongs to, its label, whether it sits in a bubble's footer and nowhere
/// interactive, and the reply ids each assistant bubble is marked with.
pub(super) const REPLY_COPY_FACTS: &str = "(() => { \
    const buttons = Array.from(document.querySelectorAll('.reply-copy')).map(b => ({ \
        reply: b.closest('.message-assistant')?.getAttribute('data-reply') ?? null, \
        label: b.innerText, title: b.title, type: b.type, \
        inFooter: b.parentElement?.classList.contains('message-footer') && b.parentElement.parentElement?.classList.contains('message-assistant'), \
        nested: b.parentElement.closest('a, details, summary, [role=button], [role=link], .markdown') !== null, \
        streaming: b.closest('.message-streaming') !== null })); \
    const bubbles = Array.from(document.querySelectorAll('.message-assistant')).map(m => ({ \
        text: m.querySelector('.markdown')?.innerText.trim() ?? '', reply: m.getAttribute('data-reply') })); \
    return { buttons, bubbles }; })()";

/// Each assistant bubble's text and whether it's outlined.
pub(super) const REPLY_OUTLINES: &str = "Array.from(document.querySelectorAll('.message-assistant')).map(m => \
    [m.querySelector('.markdown')?.innerText.trim() ?? '', getComputedStyle(m).outlineStyle !== 'none'])";

/// Waits until the bubbles outlined are those whose text starts with one of
/// `expected` (and no others).
pub(super) async fn wait_for_outlined(page: &chromiumoxide::Page, expected: &[&str], timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let bubbles: Vec<(String, bool)> = page
            .evaluate(REPLY_OUTLINES)
            .await
            .expect("read the outlines")
            .into_value()
            .expect("bubbles");
        let outlined: Vec<&str> = bubbles.iter().filter(|(_, lit)| *lit).map(|(text, _)| text.as_str()).collect();
        let matches = outlined.len() == expected.len()
            && expected.iter().all(|want| outlined.iter().any(|text| text.starts_with(want)));
        if matches {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected the bubbles starting {expected:?} outlined, got {bubbles:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `wait_for_outlined`, as a result rather than a panic.
pub(super) async fn outlined_within(page: &chromiumoxide::Page, expected: &[&str], timeout: Duration) -> Result<(), String> {
    let page = page.clone();
    let expected: Vec<String> = expected.iter().map(|e| e.to_string()).collect();
    tokio::spawn(async move {
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        wait_for_outlined(&page, &expected, timeout).await;
    })
    .await
    .map_err(|e| {
        let panic = e.into_panic();
        panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "failed".to_string())
    })
}

/// Waits until the button in reply `reply`'s bubble reads `label`.
pub(super) async fn wait_for_copy_label(page: &chromiumoxide::Page, reply: i64, label: &str, timeout: Duration) -> String {
    let js = format!("document.querySelector('.message[data-reply=\"{reply}\"] .reply-copy')?.innerText ?? ''");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let shown: String = page.evaluate(js.as_str()).await.expect("read the label").into_value().expect("a string");
        if shown == label || tokio::time::Instant::now() >= deadline {
            return shown;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) fn reply_copy_button(reply: i64) -> String {
    format!(".message[data-reply=\"{reply}\"] .reply-copy")
}
