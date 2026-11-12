use super::*;

mod depth_steps;

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

/// SME-174: a saved reply whose code block nests very deeply (D's `$(`,
/// the one grammar whose scope depth has no bound) opens like any other.
/// For each depth, in a conversation and tab of its own: the client goes
/// live, the text after the block shows, the block is highlighted with all
/// its text, its span nesting is capped at `MAX_SPAN_DEPTH`, and the page
/// still renders a live event afterwards (a wasm stack overflow is a trap
/// that leaves the client dead even where some DOM was drawn). Failures
/// are gathered per depth, so one run shows the whole series. Unfixed, the
/// client never went live from 500 on. The deepest block `highlight` takes
/// (about 32,000) isn't here: it passes too, but syntect's own parse of it
/// took about 70 s in the tier's debug bundle (SME-209).
pub(super) async fn scenario_markdown_deep_code(t: &Scenario<'_>) {
    let mut failures = Vec::new();
    for depth in [100, 500, 2_000, 8_000] {
        // A depth that hangs the tab ends alone, so the rest still run. Its
        // steps are recorded outside its timed future, so a hang still
        // names the step it hung in (SME-500).
        let steps = depth_steps::DepthSteps::new(DEEP_CODE_STEPS, Duration::from_secs(45));
        failures.extend(depth_steps::run_depth(depth, &steps, deep_code_at(t, depth, &steps)).await);
        // Closed before the next depth, however this one ended: a tab still
        // busy in a hung render would take the CPU the next one needs.
        t.close_tabs().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The steps of one depth of `scenario_markdown_deep_code`, in order.
const DEEP_CODE_STEPS: &[&str] = &["seed", "tab", "live", "text", "facts", "still_live"];

/// Whether `js` reads true on `page`, or why it couldn't be read.
async fn reads_true(page: &chromiumoxide::Page, js: &str) -> Result<bool, String> {
    page.evaluate(js).await.map_err(|e| e.to_string())?.into_value::<bool>().map_err(|e| e.to_string())
}

/// One depth of `scenario_markdown_deep_code`: what went wrong, if anything.
/// Each await runs as a step of `steps`, which the caller holds, so a depth
/// that stalls still names its step. The steps share the depth's deadline:
/// one that reaches it ends the depth here, and its timing line still
/// prints.
async fn deep_code_at(t: &Scenario<'_>, depth: usize, steps: &depth_steps::DepthSteps) -> Vec<String> {
    let mut problems = Vec::new();
    let mut timings = DeepCodeTimings::default();
    let _ = deep_code_steps(t, depth, steps, &mut problems, &mut timings).await;
    steps.finish();
    let ms = |d: Option<Duration>| d.map_or("-".to_string(), |d| d.as_millis().to_string());
    eprintln!(
        "browser tier: markdown_deep_code: depth {depth}: live in {} ms, rendered in {} ms, span depth {}; steps: {}",
        ms(timings.live_after),
        ms(timings.rendered_after),
        timings.span_depth,
        steps.report()
    );
    problems
}

/// What `deep_code_at`'s timing line reports, as far as the depth got.
#[derive(Default)]
struct DeepCodeTimings {
    live_after: Option<Duration>,
    rendered_after: Option<Duration>,
    span_depth: serde_json::Value,
}

/// `deep_code_at`'s steps; `None` once one stopped at the depth's deadline,
/// or the live check failed.
/// The polls keep the harness helpers' bounds and intervals
/// (`wait_for_live_client`: 15 s at 100 ms; `wait_for_text`: at 200 ms),
/// and also count their evaluates.
async fn deep_code_steps(
    t: &Scenario<'_>,
    depth: usize,
    steps: &depth_steps::DepthSteps,
    problems: &mut Vec<String>,
    timings: &mut DeepCodeTimings,
) -> Option<()> {
    use crate::events::{ConversationEvent, publish};
    use depth_steps::Outcome;
    let conversation = steps
        .step("seed", async {
            let conversation = t.conversation().await;
            db::create_message(t.pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "Deep code".into() }])
                .await
                .expect("seed the user's message");
            let reply = format!("Deep:\n\n```d\n{}\n```\n\nAFTER-DEEP-CODE\n", "$(".repeat(depth));
            db::create_message(t.pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: reply }])
                .await
                .expect("seed the reply");
            (conversation, Outcome::Ok)
        })
        .await?;
    let started = tokio::time::Instant::now();
    let page = steps
        .step("tab", async { (t.tab(t.url(&format!("conversation/{}", conversation.id))).await, Outcome::Ok) })
        .await?;
    let live_js = format!("document.querySelector({:?}) !== null", format!("[data-live=\"{}\"]", conversation.id));
    // A failed live evaluate ends the depth at once, as the helper's panic
    // did: re-polling it would lengthen the step on that path.
    let poll_live = steps.poll(Duration::from_secs(15), Duration::from_millis(100), depth_steps::OnError::Stop, || {
        reads_true(&page, &live_js)
    });
    let live = steps.step("live", poll_live).await?;
    timings.live_after = Some(started.elapsed());
    if let Some(e) = steps.error_in("live") {
        problems.push(format!("1: the live check failed: {e}"));
        return None;
    }
    if !live {
        problems.push("1: the client never went live".to_string());
    }
    let text_js = format!("document.body.innerText.includes({:?})", "AFTER-DEEP-CODE");
    let text = steps
        .step("text", steps.poll(Duration::from_secs(20), Duration::from_millis(200), depth_steps::OnError::Retry, || reads_true(&page, &text_js)))
        .await?;
    if !text {
        problems.push("2: the text after the block never rendered".to_string());
    }
    timings.rendered_after = Some(started.elapsed());
    // The deepest chain of scope spans (those with a class; a run of text
    // has a span of its own, with none) under the block, walked with an
    // explicit stack: a recursive walk would itself be the deep recursion.
    let facts: serde_json::Value = steps
        .step("facts", async {
            let read = page
                .evaluate(
                    "(() => { const pre = document.querySelector('.message-assistant .markdown pre.hl-code'); \
                       if (!pre) return { highlighted: false }; \
                       let deepest = 0; const stack = [[pre, 0]]; \
                       while (stack.length) { const [el, d] = stack.pop(); \
                         const here = el.tagName === 'SPAN' && el.className !== '' ? d + 1 : d; \
                         if (here > deepest) deepest = here; \
                         for (const child of el.children) stack.push([child, here]); } \
                       const text = pre.querySelector('code')?.textContent ?? pre.textContent; \
                       return { highlighted: true, dollars: text.split('$(').length - 1, deepest }; })()",
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| v.into_value().map_err(|e| e.to_string()));
            match read {
                Ok(facts) => (facts, Outcome::Ok),
                Err(e) => (serde_json::json!({ "error": e }), Outcome::Error(e)),
            }
        })
        .await?;
    timings.span_depth = facts["deepest"].clone();
    if facts["highlighted"] != true {
        problems.push(format!("3: the block isn't highlighted: {facts}"));
    } else {
        if facts["dollars"].as_u64() != Some(depth as u64) {
            problems.push(format!("4: the block lost text: {} of {depth} `$(`", facts["dollars"]));
        }
        let deepest = facts["deepest"].as_u64().unwrap_or(u64::MAX);
        if deepest > crate::highlight::MAX_SPAN_DEPTH as u64 {
            problems.push(format!("5: spans nest {deepest} deep, past {}", crate::highlight::MAX_SPAN_DEPTH));
        }
    }
    let still_live = format!("STILL-LIVE-{depth}");
    let still_live_js = format!("document.body.innerText.includes({still_live:?})");
    let shown = steps
        .step("still_live", async {
            publish(conversation.id, ConversationEvent::ReplyReset {});
            publish(conversation.id, ConversationEvent::ReplyDelta { text: still_live.clone(), offset: 0 });
            steps.poll(Duration::from_secs(10), Duration::from_millis(200), depth_steps::OnError::Retry, || reads_true(&page, &still_live_js)).await
        })
        .await?;
    if !shown {
        problems.push("6: a live event after the render never showed".to_string());
    }
    Some(())
}

/// What the tab says `prefers-color-scheme` is; an empty value lifts it.
async fn set_color_scheme(page: &chromiumoxide::Page, scheme: &str) {
    page.execute(
        chromiumoxide::cdp::browser_protocol::emulation::SetEmulatedMediaParams::builder()
            .feature(chromiumoxide::cdp::browser_protocol::emulation::MediaFeature::new("prefers-color-scheme", scheme))
            .build(),
    )
    .await
    .expect("set the colour scheme");
}

/// SME-386 F3: the link in a user bubble is underlined and meets WCAG
/// AA's 4.5:1 against the bubble. It was the browser's default blue on
/// the bubble's near-black, 2.01:1.
async fn assert_user_bubble_link_reads(page: &chromiumoxide::Page, link: &str, scheme: &str) {
    let facts: serde_json::Value = page
        .evaluate(format!(
            "(() => {{ const lum = c => {{ const [r, g, b] = c.match(/[\\d.]+/g).slice(0, 3).map(Number).map(v => {{ v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }}); return 0.2126 * r + 0.7152 * g + 0.0722 * b; }}; \
             const a = document.querySelector({link:?}); const fg = getComputedStyle(a).color; const bg = getComputedStyle(a.closest('.message-user')).backgroundColor; \
             const [hi, lo] = [lum(fg), lum(bg)].sort((x, y) => y - x); \
             return {{ fg, bg, ratio: (hi + 0.05) / (lo + 0.05), line: getComputedStyle(a).textDecorationLine }}; }})()"
        ))
        .await
        .expect("read the link's colours")
        .into_value()
        .expect("facts");
    let ratio = facts["ratio"].as_f64().expect("a ratio");
    assert!(ratio >= 4.5, "{scheme}: a reference in a user bubble is too faint: {facts}");
    assert_eq!(facts["line"], "underline", "{scheme}: it should read as a link: {facts}");
}

/// SME-384: `agent #N` / `conversation #N` references render as links
/// to that conversation — in an assistant reply (the markdown path) and
/// in a report header shown as a plain user bubble (the path smelt's
/// own delivered words take) — and clicking one navigates the same tab.
/// A reference in backticks stays code, a bare `#255` stays text, and a
/// link to a deleted id lands on the doesn't-exist state.
pub(super) async fn scenario_agent_reference_links(t: &Scenario<'_>) {
    let a = t.conversation().await;
    let b = t.conversation().await;
    let c = t.conversation().await;
    let (a_id, b_id, c_id) = (a.id, b.id, c.id);
    // B and C get a message of their own so the scenario can tell the
    // conversation it navigated to from A's transcript.
    db::create_message(t.pool, b_id, "user", &[anthropic::ContentBlock::Text { text: "B-TRANSCRIPT-MARKER".into() }])
        .await
        .expect("seed B");
    db::create_message(t.pool, c_id, "user", &[anthropic::ContentBlock::Text { text: "C-TRANSCRIPT-MARKER".into() }])
        .await
        .expect("seed C");
    // A report header as `turn::notify`'s `agent_message_text` delivers
    // it: an unmarked user-role message.
    db::create_message(
        t.pool,
        a_id,
        "user",
        &[anthropic::ContentBlock::Text { text: format!("Report from agent #{b_id} (smelt:developer):\n\nAll done: 42.") }],
    )
    .await
    .expect("seed the report header");
    const DELETED: i64 = 987_654_321;
    db::create_message(
        t.pool,
        a_id,
        "assistant",
        &[anthropic::ContentBlock::Text {
            text: format!("See conversation #{c_id} for the details, and `agent #{b_id}` in code, and See #255, and agent #{DELETED} is gone."),
        }],
    )
    .await
    .expect("seed the reply");
    let page = t.tab(t.url(&format!("conversation/{a_id}"))).await;
    wait_for_live_client(&page, a_id).await;

    // The reply's reference to C is a link through the router...
    let reply_link = format!(".message-assistant .markdown a[href=\"/conversation/{c_id}\"]");
    wait_for_element(&page, &reply_link, Duration::from_secs(10)).await;
    // ...and so is the header's reference to B, which before SME-384 was
    // one text node with no anchor anywhere in the bubble.
    let header_link = format!(".message-user .message-text a[href=\"/conversation/{b_id}\"]");
    wait_for_element(&page, &header_link, Duration::from_secs(10)).await;
    assert_user_bubble_link_reads(&page, &header_link, "light").await;
    set_color_scheme(&page, "dark").await;
    assert_user_bubble_link_reads(&page, &header_link, "dark").await;
    set_color_scheme(&page, "").await;

    let facts: serde_json::Value = page
        .evaluate(
            "(() => ({ userHref: document.querySelector('.message-user .message-text a')?.getAttribute('href') ?? null, \
               userTarget: document.querySelector('.message-user .message-text a')?.getAttribute('target') ?? null, \
               userAnchors: document.querySelectorAll('.message-user .message-text a').length, \
               replyHrefs: Array.from(document.querySelectorAll('.message-assistant .markdown a')).map(x => x.getAttribute('href')), \
               code: document.querySelector('.message-assistant .markdown code')?.innerText ?? null }))()",
        )
        .await
        .expect("read the links")
        .into_value()
        .expect("facts");
    assert_eq!(facts["userHref"], format!("/conversation/{b_id}"), "the header links to B: {facts}");
    assert!(facts["userTarget"].is_null(), "a reference opens in the same tab: {facts}");
    assert_eq!(facts["userAnchors"], 1, "{facts}");
    assert_eq!(
        facts["replyHrefs"],
        serde_json::json!([format!("/conversation/{c_id}"), format!("/conversation/{DELETED}")]),
        "only the two real references link — the backticked one stays code and #255 stays text: {facts}"
    );
    assert_eq!(facts["code"], format!("agent #{b_id}"), "the backtick control: {facts}");

    // Clicking the header's link navigates this very tab to B.
    click_when_present(&page, &header_link, Duration::from_secs(10)).await;
    assert!(wait_for_text(&page, "B-TRANSCRIPT-MARKER", Duration::from_secs(10)).await, "the header link didn't open B");
    let path: String = page.evaluate("location.pathname").await.expect("read the path").into_value().expect("a string");
    assert_eq!(path, format!("/conversation/{b_id}"), "{facts}");

    // Back to A; the reply's link navigates the same way.
    page.evaluate("history.back()").await.expect("back to A");
    assert!(wait_for_text(&page, "See conversation", Duration::from_secs(10)).await, "back didn't return to A");
    click_when_present(&page, &reply_link, Duration::from_secs(10)).await;
    assert!(wait_for_text(&page, "C-TRANSCRIPT-MARKER", Duration::from_secs(10)).await, "the reply link didn't open C");
    let path: String = page.evaluate("location.pathname").await.expect("read the path").into_value().expect("a string");
    assert_eq!(path, format!("/conversation/{c_id}"), "{facts}");

    // Back to A; a reference to a deleted id lands on the doesn't-exist
    // state without killing the tab.
    page.evaluate("history.back()").await.expect("back to A");
    assert!(wait_for_text(&page, "Report from agent", Duration::from_secs(10)).await, "back didn't return to A");
    let dead_link = format!(".message-assistant .markdown a[href=\"/conversation/{DELETED}\"]");
    click_when_present(&page, &dead_link, Duration::from_secs(10)).await;
    assert!(
        wait_for_text(&page, "doesn't exist", Duration::from_secs(10)).await,
        "a deleted reference should land on the doesn't-exist state"
    );
    let alive: bool = page
        .evaluate("document.body.innerText.length > 0")
        .await
        .expect("the tab is alive")
        .into_value()
        .expect("a bool");
    assert!(alive, "the tab broke on the dead reference");
}
