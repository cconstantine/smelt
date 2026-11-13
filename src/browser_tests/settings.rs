use super::*;

/// SME-51 B11: switching conversations resets what belonged to the one
/// left: its context detail view doesn't stay open over the next
/// conversation.
pub(super) async fn scenario_switch_resets_context_detail(t: &Scenario<'_>) {
    let leaving = t.conversation().await;
    let arriving = t.conversation().await;
    seed_usage(t.pool, leaving.id, 1_000, 100).await;
    let switching = t.tab(t.url(&format!("conversation/{}", leaving.id))).await;
    click_when_present(&switching, ".context-usage-bar", Duration::from_secs(10)).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 1, Duration::from_secs(5)).await,
        "the context detail should open"
    );
    click_conversation(&switching, arriving.id).await;
    assert!(
        wait_for_count(&switching, ".context-detail-overlay", 0, Duration::from_secs(5)).await,
        "the first conversation's context detail stayed open after switching"
    );
}

/// Scenario 24 (SME-72): the model picker above the message box says which
/// model the conversation runs on, and choosing another in one tab shows in
/// every tab on it, with no reload.
pub(super) async fn scenario_model_picker(t: &Scenario<'_>) {
    let pool = t.pool;
    let picking = t.conversation().await;
    let provider_name = db::get_inference_provider(pool, *MOCK_PROVIDER.get().expect("the mock provider"))
        .await
        .expect("read the mock provider")
        .expect("the mock provider exists")
        .name;
    let first_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    let second_tab = t.tab(t.url(&format!("conversation/{}", picking.id))).await;
    wait_for_live_client(&first_tab, picking.id).await;
    wait_for_live_client(&second_tab, picking.id).await;
    let current = format!("{provider_name} \u{b7} {}", t.mock.model);
    for tab in [&first_tab, &second_tab] {
        assert!(
            wait_for_text(tab, &current, Duration::from_secs(10)).await,
            "the picker should show the conversation's model, {current:?}"
        );
    }
    click_when_present(&first_tab, ".model-picker-button", Duration::from_secs(5)).await;
    // Clear the field and type, until it holds exactly the new id: a
    // re-render between selecting the old text and typing would
    // otherwise leave the two run together.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let model_field = wait_for_element(&first_tab, ".model-chooser-model", Duration::from_secs(5)).await;
        model_field.focus().await.expect("focus the model field");
        first_tab
            .evaluate("document.querySelector('.model-chooser-model').select()")
            .await
            .expect("select the current model id");
        model_field.type_str("other-model").await.expect("type a model id");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let typed: String = first_tab
            .evaluate("document.querySelector('.model-chooser-model')?.value ?? ''")
            .await
            .expect("read the model field")
            .into_value()
            .expect("a string");
        if typed == "other-model" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the model field never held just the new id: {typed:?}");
    }
    click_when_present(&first_tab, ".model-chooser button[type=\"submit\"]", Duration::from_secs(5)).await;
    let chosen = format!("{provider_name} \u{b7} other-model");
    if !wait_for_text(&first_tab, &chosen, Duration::from_secs(10)).await {
        let shown: String = first_tab
            .evaluate("document.querySelector('.model-picker')?.innerText ?? ''")
            .await
            .expect("read the picker")
            .into_value()
            .expect("a string");
        panic!("the choosing tab should show the new model; the picker says {shown:?}");
    }
    assert!(
        wait_for_text(&second_tab, &chosen, Duration::from_secs(10)).await,
        "the other tab should show the new model live"
    );
    let stored = db::get_conversation_model(pool, picking.id)
        .await
        .expect("read the conversation's model")
        .expect("the conversation exists");
    assert_eq!(stored.model.as_deref(), Some("other-model"));
}

/// Puts the server's build id back to its own when dropped, so a failing
/// stale-bundle scenario doesn't leave every later page asking for a
/// reload.
pub(super) struct ResetBuildId;

impl Drop for ResetBuildId {
    fn drop(&mut self) {
        crate::api::version::test_override::set(None);
    }
}

/// Scenario 26 (SME-43): a tab running an older bundle than the server.
pub(super) async fn scenario_stale_bundle(t: &Scenario<'_>) {
    let _reset = ResetBuildId;
    // (a) An event type the bundle doesn't know is skipped: the stream stays
    // up (one connection's pull, not a second after a reconnect), and the
    // next event still arrives live. It also proves the server is newer,
    // so the tab asks for a reload.
    let stale = t.conversation().await;
    let stale_url = t.url(&format!("conversation/{}", stale.id));
    let stale_tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&stale_tab, stale.id).await;
    assert!(
        !banner_shown(&stale_tab).await,
        "a page built from the server's own tree shouldn't ask for a reload"
    );
    crate::events::publish(stale.id, crate::events::ConversationEvent::BrowserTestAddedLater {});
    crate::events::publish(
        stale.id,
        crate::events::ConversationEvent::TodoListUpdate {
            items: vec![anthropic::tools::TodoItem {
                content: "scenario 26 todo".to_string(),
                status: anthropic::tools::TodoStatus::Pending,
            }],
        },
    );
    assert!(
        wait_for_text(&stale_tab, "scenario 26 todo", Duration::from_secs(5)).await,
        "an event after one of an unknown type should still arrive live"
    );
    assert_eq!(
        live_pulls(&stale_tab, stale.id).await,
        Some(1),
        "an unknown event type shouldn't end the stream"
    );
    assert!(
        wait_for_text(&stale_tab, "smelt was updated", Duration::from_secs(5)).await,
        "an unknown event type should ask for a reload"
    );
    stale_tab.close().await.expect("close the stale tab");

    // (b) The server is redeployed: the tab's stream drops, and the
    // reconnect finds another build. The Sandboxes page, whose stream
    // is the app-wide one, notices too. Reloading clears it.
    let tab = t.tab(stale_url.as_str()).await;
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "no reload banner before the server changes");
    crate::api::version::test_override::set(Some("a-newer-build"));
    crate::events::forget(stale.id);
    assert!(
        wait_for_text(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "a reconnect to a newer server should ask for a reload"
    );
    let pods_tab = t.tab(t.url("pods")).await;
    assert!(
        wait_for_text(&pods_tab, "smelt was updated", Duration::from_secs(10)).await,
        "the Sandboxes page should notice a newer server too"
    );
    pods_tab.close().await.expect("close the Sandboxes page");
    crate::api::version::test_override::set(None);
    click_when_present(&tab, ".stale-bundle-banner button", Duration::from_secs(5)).await;
    assert!(
        wait_for_text_gone(&tab, "smelt was updated", Duration::from_secs(10)).await,
        "Reload should load the page again"
    );
    wait_for_live_client(&tab, stale.id).await;
    assert!(!banner_shown(&tab).await, "the reloaded page is current");
}

/// Scenario 27 (SME-83): scrolling the transcript up a little leaves it
/// there. Every scroll event set the stuck-to-bottom flag, which reran the
/// auto-scroll effect, so a scroll that stayed within the 32px slack (a
/// trackpad's first small steps) snapped back.
pub(super) async fn scenario_transcript_scroll(t: &Scenario<'_>) {
    let long_conversation = t.conversation().await;
    for i in 0..40 {
        db::create_message(
            t.pool,
            long_conversation.id,
            if i % 2 == 0 { "user" } else { "assistant" },
            &[anthropic::ContentBlock::Text { text: format!("scroll filler message {i}") }],
        )
        .await
        .expect("seed a message");
    }
    let long_page = t.tab(t.url(&format!("conversation/{}", long_conversation.id))).await;
    wait_for_live_client(&long_page, long_conversation.id).await;
    // Settled at the bottom: the same distance on two reads apart.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = f64::NAN;
    loop {
        let distance = transcript_distance_from_bottom(&long_page).await;
        if distance <= 1.0 && distance == last {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transcript never settled at its bottom: {distance}px away"
        );
        last = distance;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    long_page
        .evaluate("(() => { const m = document.querySelector('.messages'); m.scrollTop = m.scrollTop - 10; })()")
        .await
        .expect("scroll the transcript up");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let distance = transcript_distance_from_bottom(&long_page).await;
    assert!(
        (8.0..=12.0).contains(&distance),
        "a 10px scroll up should stay where it was put, but the transcript is {distance}px from its bottom"
    );
}

/// Keeps what a page reports going wrong, where a scenario can read it.
const PAGE_ERRORS: &str = "window.__smeltErrors = [];\
    addEventListener('error', e => window.__smeltErrors.push(String(e.message)));\
    addEventListener('unhandledrejection', e => window.__smeltErrors.push(String(e.reason)));\
    { const original = console.error; console.error = (...args) => { let said; try { said = args.join(' ').slice(0, 300); } catch { said = 'unprintable console.error'; } window.__smeltErrors.push(said); original(...args); }; }";

/// Clears the browser's finished requests, so `row_missing_report`'s count is
/// this save's alone and not an earlier save's on the same document.
async fn forget_earlier_saves(page: &chromiumoxide::Page) {
    page.evaluate("performance.clearResourceTimings()").await.expect("clear the resource timings");
}

/// SME-371: what the page and the store held when a saved secret's row didn't
/// appear within its wait. Read together, the fields should name the mechanism,
/// none of them alone. `stored` with the button still on "Adding..." and no
/// finished answer: the save committed and its response hadn't come by then.
/// No finished answer and not stored: the request may never have left. A
/// finished answer the page then used shows in the button and the page text, a
/// server's error among them; one with the button still on "Adding..." was never
/// used. In this debug build a Rust panic goes through Dioxus's hook, so it
/// should show first as its text in the page errors, with the toast shown:
/// `aria-hidden` is the difference between `dx`'s dev template, always in the
/// DOM off-screen, and a toast shown at runtime. Only a deliberately held
/// response has been read so far.
async fn row_missing_report(t: &Scenario<'_>, page: &chromiumoxide::Page, what: &str, name: &str) -> String {
    let seen: String = page
        .evaluate("(() => [document.querySelectorAll('.secret-row').length, document.querySelector('#secret-value')?.value ?? 'no field', document.querySelector('.secrets-add-form button')?.textContent ?? 'no button', performance.getEntriesByType('resource').filter(e => e.name.endsWith('/api/secrets/save')).length, (window.__smeltErrors ?? ['no error hook']).join(' ; '), document.querySelector('#__dx-toast')?.getAttribute('aria-hidden') ?? 'no toast', document.querySelector('#main')?.innerText.slice(0, 800) ?? 'no #main'].join(' | '))()")
        .await
        .ok()
        .and_then(|v| v.into_value().ok())
        .unwrap_or_default();
    let stored = crate::db::list_named_secrets(t.pool).await.map(|s| s.iter().any(|s| s.name == name));
    format!("the {what}'s row should appear; rows | field | button | save answers finished | page errors | toast aria-hidden | page text: {seen:?}; stored: {stored:?}")
}

/// T12 (SME-242): the Secrets page adds a secret, changes its value in
/// place, lists only the server's display prefix — never the value —
/// and deletes it; and a row fits 375px.
pub(super) async fn scenario_secrets_page(t: &Scenario<'_>) {
    /// The server-rendered form accepts typing before hydration with no
    /// handlers attached, so type and read back until the field holds just
    /// what was typed, the way the model picker's scenario does (SME-118).
    async fn type_and_read_back(page: &chromiumoxide::Page, selector: &str, text: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let field = wait_for_element(page, selector, Duration::from_secs(5)).await;
            field.focus().await.expect("focus the field");
            page.evaluate(format!("document.querySelector('{selector}').select()"))
                .await
                .expect("select the old text");
            field.type_str(text).await.expect("type");
            tokio::time::sleep(Duration::from_millis(200)).await;
            let typed: String = page
                .evaluate(format!("document.querySelector('{selector}')?.value ?? ''"))
                .await
                .expect("read the field")
                .into_value()
                .expect("a string");
            if typed == text {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the field {selector} never held just the typed text: {typed:?}");
        }
    }

    let value = "top-scenario-secret-value-0123456789"; // 36 bytes: prefix "top-", 32 hidden
    let name = unique_id("key");
    let page = t.tab(t.url("secrets")).await;
    // Every document this tab loads keeps its errors, for the paste leg's
    // report below (the reload before it picks this up).
    page.evaluate_on_new_document(
        chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams::new(PAGE_ERRORS.to_string()),
    )
    .await
    .expect("keep the page's errors");
    wait_for_resource(&page, "/api/secrets").await;
    type_and_read_back(&page, "#secret-name", &name).await;
    type_and_read_back(&page, "#secret-value", value).await;
    click_when_present(&page, ".secrets-add-form button[type=\"submit\"]", Duration::from_secs(5)).await;
    assert!(wait_for_count(&page, ".secret-row", 1, Duration::from_secs(10)).await, "the added secret's row should appear");
    let row: Vec<String> = page
        .evaluate("(() => { const r = document.querySelector('.secret-row'); return [r.querySelector('.sandbox-volume-name').textContent, r.querySelector('.secret-shown').textContent]; })()")
        .await
        .expect("read the row")
        .into_value()
        .expect("two strings");
    assert_eq!(row[0], name, "the row names the secret");
    let shown = &row[1];
    assert!(shown.starts_with("top-…") && shown.contains("(36 bytes)"), "the row shows the four-character display prefix and the exact byte length: {shown:?}");
    let body: String = page
        .evaluate("document.body.innerText")
        .await
        .expect("read the page")
        .into_value()
        .expect("text");
    assert!(!body.contains(&value[6..]), "beyond the display prefix none of the value is on the page");

    // Change the stored value in place: the row's change control sends
    // the row's existing name on the id path (renaming is delete and
    // add, Trade-off 7), the row stays one row, and after it the page
    // shows the new display — with no trace of the old value's prefix.
    let new_value = "renewed-scenario-secret-0123456789"; // 34 bytes: prefix "rene", 30 hidden
    click_when_present(&page, ".secret-row .secret-change", Duration::from_secs(5)).await;
    type_and_read_back(&page, ".secret-change-input", new_value).await;
    click_when_present(&page, ".secret-row .secret-change-confirm", Duration::from_secs(5)).await;
    let shown_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (changed, shown_now) = loop {
        let text: String = page
            .evaluate("document.querySelector('.secret-row .secret-shown')?.textContent ?? ''")
            .await
            .expect("read the row")
            .into_value()
            .expect("a string");
        if text == "rene… (34 bytes)" {
            break (true, text);
        }
        if tokio::time::Instant::now() >= shown_deadline {
            break (false, text);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(changed, "after the change the row shows the new display, not {shown_now:?}");
    assert!(wait_for_count(&page, ".secret-row", 1, Duration::from_secs(5)).await, "a change keeps it one row");
    let body: String = page
        .evaluate("document.body.innerText")
        .await
        .expect("read the page")
        .into_value()
        .expect("text");
    assert!(!body.contains("top-"), "the replaced value's old display prefix is gone from the page: {body}");
    assert!(!body.contains(&new_value[4..]), "beyond the new display prefix none of the new value is on the page");

    // Delete is two-step: arm, then the named confirm.
    click_when_present(&page, ".secret-row .secret-delete", Duration::from_secs(5)).await;
    click_when_present(&page, ".secret-row .secret-delete-confirm", Duration::from_secs(5)).await;
    assert!(wait_for_count(&page, ".secret-row", 0, Duration::from_secs(10)).await, "the confirm removes the row");

    // SME-334 review (finding 1): an empty value must be refused before
    // sending, saying the server's rule — the voice SME-242's checklist
    // clause promises. Two things hid it: a `required` attribute on the
    // value field makes Chromium block the submit event itself (the native
    // bubble shows, the page handler never runs), and the page only ever
    // rendered load_error, so every action_error the handlers set — this
    // refusal included — was said to an invisible signal. Red on either.
    // Reload first: the value signal must start empty, and typing the name
    // through the helper doubles as the hydration check.
    page.reload().await.expect("reload the page");
    wait_for_resource(&page, "/api/secrets").await;
    // The error hook keeps something, and the page's own call doesn't throw,
    // for what can't be joined into a string (SME-371). This reload is where
    // the hook first applies. Only this check's own entries are taken back:
    // an error the page survived before it stays for the report below, as
    // the one it seeds here shows, before taking that back too.
    let unjoinable: String = page
        .evaluate("(() => { try { console.error('an error the page survived'); const before = window.__smeltErrors.length; console.error(Symbol('s')); console.error(Object.create(null)); const added = window.__smeltErrors.splice(before).join(' ; '); return added + ' | kept: ' + window.__smeltErrors.pop(); } catch (e) { return 'threw: ' + e; } })()")
        .await
        .expect("call the hook")
        .into_value()
        .expect("a string");
    assert_eq!(unjoinable, "unprintable console.error ; unprintable console.error | kept: an error the page survived");
    let empty_name = unique_id("empty");
    type_and_read_back(&page, "#secret-name", &empty_name).await;
    click_when_present(&page, ".secrets-add-form button[type=\"submit\"]", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&page, "A secret value can't be empty.", Duration::from_secs(5)).await,
        "an empty value is refused before sending, in the server's own words"
    );
    // The store, not the row count: the count was already 0, so it could
    // not tell a late row from none.
    let stored = crate::db::list_named_secrets(t.pool).await.expect("list the secrets");
    assert!(!stored.iter().any(|s| s.name == empty_name), "the refused add stored nothing");

    // SME-334: the paste leg — the one automated test that watches the
    // control itself. A multi-line secret goes in the way a paste goes in
    // — the edit path, `execCommand('insertText')`, which is what the
    // browser's paste takes — and the field must hold every newline, the
    // row must count them, `shown` must still hold the exact byte count,
    // and none of the value may reach the page. Red on the unfixed
    // single-line input: its edit path replaces every newline with a
    // space — the exact flattening this ticket filed.
    let yaml = "apiVersion: v1\nclusters:\n\nkind: list\n"; // 37 bytes: prefix "apiV…", 33 hidden
    let paste_name = unique_id("kube");
    type_and_read_back(&page, "#secret-name", &paste_name).await;
    let paste_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let field = wait_for_element(&page, "#secret-value", Duration::from_secs(5)).await;
        field.focus().await.expect("focus the value field");
        page.evaluate("document.querySelector('#secret-value').select()")
            .await
            .expect("select the old text");
        page.evaluate(format!(
            "document.execCommand('insertText', false, \"{}\")",
            yaml.replace('\n', "\\n")
        ))
        .await
        .expect("paste multi-line text the way a paste does");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pasted: String = page
            .evaluate("document.querySelector('#secret-value')?.value ?? ''")
            .await
            .expect("read the field")
            .into_value()
            .expect("a string");
        if pasted == yaml {
            break;
        }
        assert!(tokio::time::Instant::now() < paste_deadline, "the pasted newlines did not survive the field: {pasted:?}");
    }
    forget_earlier_saves(&page).await;
    click_when_present(&page, ".secrets-add-form button[type=\"submit\"]", Duration::from_secs(5)).await;
    if !wait_for_count(&page, ".secret-row", 1, Duration::from_secs(10)).await {
        panic!("{}", row_missing_report(t, &page, "pasted secret", &paste_name).await);
    }
    // Said on a pass too: an error the page survived is a lead on the one it
    // wouldn't (SME-371).
    let page_errors: String = page
        .evaluate("(window.__smeltErrors ?? ['no error hook']).join(' ; ')")
        .await
        .expect("read the page's errors")
        .into_value()
        .expect("a string");
    if !page_errors.is_empty() {
        eprintln!("browser tier: secrets_page: page errors after the paste: {page_errors}");
    }
    let pasted_row: Vec<String> = page
        .evaluate("(() => { const r = document.querySelector('.secret-row'); return [r.querySelector('.secret-shown').textContent, r.querySelector('.secret-lines')?.textContent ?? 'no .secret-lines', r.title]; })()")
        .await
        .expect("read the row")
        .into_value()
        .expect("three strings");
    assert!(pasted_row[0].starts_with("apiV…") && pasted_row[0].contains("(37 bytes)"), "shown holds the exact byte count of the pasted value: {:?}", pasted_row[0]);
    assert_eq!(pasted_row[1], "4 lines", "the row counts the pasted value's lines");
    assert!(pasted_row[2].contains("4 lines"), "the row's title carries the count too: {}", pasted_row[2]);
    let body: String = page
        .evaluate("document.body.innerText")
        .await
        .expect("read the page")
        .into_value()
        .expect("text");
    // Line by line: innerText folds the page's whitespace, so a leaked
    // multi-line value would never match the value whole.
    for leaked in ["Version: v1", "clusters:", "kind: list"] {
        assert!(!body.contains(leaked), "beyond the display prefix none of the pasted value is on the page: {leaked:?}");
    }
    // Two-step delete again, so the 375px leg below starts from none.
    click_when_present(&page, ".secret-row .secret-delete", Duration::from_secs(5)).await;
    click_when_present(&page, ".secret-row .secret-delete-confirm", Duration::from_secs(5)).await;
    assert!(wait_for_count(&page, ".secret-row", 0, Duration::from_secs(10)).await, "the pasted secret's row deletes again");

    // SME-371: a save with a live pod whose install won't finish. Its
    // conversation's install lock is held, as a stalled exec holds it for up
    // to a minute. A save has committed before it starts on the pods, so on
    // the unfixed code the page sat on "Adding..." with the value still in
    // the field. A leg of its own: the hold spends the answer's whole
    // deadline, which the paste leg's window shouldn't have to pay for.
    let held_name = unique_id("held");
    type_and_read_back(&page, "#secret-name", &held_name).await;
    type_and_read_back(&page, "#secret-value", value).await;
    let stuck = t.conversation().await;
    let stuck_pod = db::create_sandbox_pod(t.pool, stuck.id).await.expect("a live pod row");
    super::deliberately_held(stuck_pod.id);
    let (release_install, released) = tokio::sync::oneshot::channel::<()>();
    let (install_held, is_held) = tokio::sync::oneshot::channel::<()>();
    let stuck_install = tokio::spawn(crate::credentials::with_install_lock(stuck.id, async move {
        let _ = install_held.send(());
        let _ = released.await;
    }));
    is_held.await.expect("the install lock is held");
    forget_earlier_saves(&page).await;
    click_when_present(&page, ".secrets-add-form button[type=\"submit\"]", Duration::from_secs(5)).await;
    // Read while the install is still held, and said only after it's released.
    let missing = if wait_for_count(&page, ".secret-row", 1, Duration::from_secs(10)).await {
        None
    } else {
        Some(row_missing_report(t, &page, "held save", &held_name).await)
    };
    let _ = release_install.send(());
    stuck_install.await.expect("the held install lock");
    db::terminate_sandbox_pod(t.pool, stuck_pod.id).await.expect("end the stuck pod's row");
    if let Some(missing) = missing {
        panic!("{missing}");
    }
    // The leg proves nothing unless its hold really outlasted the deadline.
    let firings = crate::credentials::install_deadline_firings_besides(&[]);
    assert!(firings.iter().any(|pods| pods.contains(&stuck_pod.id)), "the hold never fired the deadline: {firings:?}");
    click_when_present(&page, ".secret-row .secret-delete", Duration::from_secs(5)).await;
    click_when_present(&page, ".secret-row .secret-delete-confirm", Duration::from_secs(5)).await;
    assert!(wait_for_count(&page, ".secret-row", 0, Duration::from_secs(10)).await, "the held save's row deletes again");

    // A seeded row at 375px: it renders, and the page doesn't scroll sideways.
    let seeded = db::create_named_secret(t.pool, &unique_id("phone"), "phone-value-0123456789").await.expect("seed a secret");
    page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(375, 700, 2.0, true))
        .await
        .expect("emulate a phone");
    page.reload().await.expect("reload at 375");
    wait_for_resource(&page, "/api/secrets").await;
    let rows_at_phone = wait_for_count(&page, ".secret-row", 1, Duration::from_secs(10)).await;
    let overflow: f64 = page
        .evaluate("document.documentElement.scrollWidth - document.documentElement.clientWidth")
        .await
        .expect("measure the sideways overflow")
        .into_value()
        .expect("a number");
    db::delete_named_secret(t.pool, seeded.id).await.expect("delete the seeded secret");
    assert!(rows_at_phone, "a secret's row should render at 375px");
    assert!(overflow <= 1.0, "the Secrets page should not scroll sideways at 375px: {overflow}");
}
