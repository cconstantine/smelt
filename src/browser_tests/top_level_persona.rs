//! SME-481: the start screen's "Start as a persona" picker. A conversation
//! is seeded with a ready checkout on a trusted remote, and a fake checkout
//! (`agents::test_fakes`) holds its persona files, so the set reads them
//! without a pod.

use super::*;
use std::sync::Arc;

/// A persona file named `name`, described by `description`.
fn persona_md(name: &str, description: &str, smelt: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n{smelt}---\nWork as {name}.\n")
}

/// The persona files of a fake checkout, by path relative to it.
struct FakePersonas(Vec<(String, String)>);

type Boxed<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

impl crate::agents::ChildCheckout for FakePersonas {
    fn parent_commit<'a>(&'a self, _: &'a sqlx::PgPool, _: i64, _: &'a str) -> Boxed<'a, Result<(String, String), String>> {
        Box::pin(async { Err("this fake has no commit".to_string()) })
    }
    fn parent_persona_paths<'a>(&'a self, _: &'a sqlx::PgPool, _: i64, _: &'a str) -> Boxed<'a, Result<Vec<String>, String>> {
        Box::pin(async move { Ok(self.0.iter().map(|(path, _)| path.clone()).collect()) })
    }
    fn clone_repo<'a>(
        &'a self,
        _: &'a sqlx::PgPool,
        _: i64,
        _: &'a db::ConversationRepo,
        _: &'a crate::git::CloneTarget,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async { Err("this fake doesn't clone".to_string()) })
    }
    fn parent_file<'a>(
        &'a self,
        _: &'a sqlx::PgPool,
        _: i64,
        _: &'a str,
        rel: &'a str,
    ) -> Boxed<'a, Result<Option<db::InstructionsFile>, String>> {
        Box::pin(async move { Ok(self.file(rel)) })
    }
    fn restart_pod<'a>(&'a self, _: &'a sqlx::PgPool, _: i64) -> Boxed<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn reprovision<'a>(
        &'a self,
        _: &'a sqlx::PgPool,
        _: i64,
        _: &'a db::ConversationRepo,
        _: &'a crate::git::CloneTarget,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async { Err("this fake doesn't clone".to_string()) })
    }
    fn read_file<'a>(
        &'a self,
        _: &'a sqlx::PgPool,
        _: i64,
        _: &'a str,
        rel: &'a str,
    ) -> Boxed<'a, Result<Option<db::InstructionsFile>, String>> {
        Box::pin(async move { Ok(self.file(rel)) })
    }
    fn stop_pod<'a>(&'a self, _: &'a sqlx::PgPool, _: i64) -> Boxed<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

impl FakePersonas {
    fn file(&self, rel: &str) -> Option<db::InstructionsFile> {
        self.0.iter().find(|(path, _)| path == rel).map(|(_, content)| db::InstructionsFile {
            file_bytes: content.len() as i64,
            content: content.clone(),
            hash: "h".to_string(),
            commit: None,
        })
    }
}

/// A conversation on the mock model with a ready checkout `dir` of a remote
/// of its own, trusted, holding `personas` (`(name, file)`).
async fn conversation_with_personas(t: &Scenario<'_>, dir: &str, personas: &[(&str, String)]) -> i64 {
    let id = t.conversation().await.id;
    let url = format!("https://example.com/sme481/{dir}.git");
    let key = format!("example.com/sme481/{dir}");
    let repo = db::create_conversation_repo(t.pool, id, &url, &key, None, dir).await.expect("a repo row");
    db::set_repo_cloned(t.pool, repo.id, "main", Some("c0ffee0000000000000000000000000000000481")).await.expect("ready");
    db::set_repo_trust(t.pool, &key, true).await.expect("trust");
    let files = personas.iter().map(|(name, file)| (format!(".agents/personas/{name}.md"), file.clone())).collect();
    crate::agents::test_fakes::register(id, Arc::new(FakePersonas(files)));
    id
}

/// Picks `label` in the picker's select, as a user's change would.
async fn pick(page: &chromiumoxide::Page, label: &str) {
    page.evaluate(format!(
        "(() => {{ const s = document.querySelector('#persona-picker'); s.value = '{label}'; \
         s.dispatchEvent(new Event('change', {{ bubbles: true }})); }})()"
    ))
    .await
    .expect("pick a persona");
}

async fn text_of(page: &chromiumoxide::Page, selector: &str) -> String {
    page.evaluate(format!("document.querySelector(\"{selector}\")?.innerText ?? ''"))
        .await
        .expect("read the text")
        .into_value()
        .expect("text")
}

/// B1: the picker lists both personas; picking one shows its description;
/// the button sets it, the picker becomes the "runs as" line, the sidebar
/// row shows the persona, and a second tab shows the same line live. Once a
/// message is sent, the start screen and the picker are gone.
pub(super) async fn scenario_persona_picker_starts_a_conversation_as_a_persona(t: &Scenario<'_>) {
    let dir = unique_id("b1");
    let personas = [
        ("driver", persona_md("driver", "Drives one ticket to done.", "")),
        ("reviewer", persona_md("reviewer", "Reviews one diff.", "smelt:\n  tools:\n    class: read-only\n")),
    ];
    let id = conversation_with_personas(t, &dir, &personas).await;
    let page = t.tab(t.url(&format!("conversation/{id}"))).await;
    wait_for_live_client(&page, id).await;
    let other = t.tab(t.url(&format!("conversation/{id}"))).await;
    wait_for_live_client(&other, id).await;
    wait_for_element(&page, "#persona-picker", Duration::from_secs(15)).await;
    let listed: Vec<String> = page
        .evaluate("Array.from(document.querySelectorAll('#persona-picker option')).map(o => o.value)")
        .await
        .expect("read the options")
        .into_value()
        .expect("strings");
    assert_eq!(listed, ["".to_string(), format!("{dir}:driver"), format!("{dir}:reviewer")], "{listed:?}");
    pick(&page, &format!("{dir}:reviewer")).await;
    assert!(wait_for_text(&page, "Reviews one diff.", Duration::from_secs(5)).await, "its description shows");
    click_when_present(&page, "#persona-picker-set", Duration::from_secs(5)).await;
    wait_for_element(&page, "#persona-set", Duration::from_secs(15)).await;
    let line = text_of(&page, "#persona-set").await;
    assert!(line.starts_with(&format!("This conversation runs as {dir}:reviewer")), "{line}");
    assert!(page.evaluate("!document.querySelector('#persona-picker')").await.expect("read").into_value::<bool>().expect("bool"));
    let row = format!(".conversation-item[data-conversation-id='{id}'] .agent-persona");
    wait_for_element(&page, &row, Duration::from_secs(10)).await;
    assert_eq!(text_of(&page, &row).await, format!("{dir}:reviewer"));
    wait_for_element(&other, "#persona-set", Duration::from_secs(15)).await;
    assert!(text_of(&other, "#persona-set").await.contains(&format!("{dir}:reviewer")), "the other tab is told");

    crate::turn::start_turn(t.pool.clone(), id, "Hello.".to_string()).await.expect("the send");
    assert!(wait_for_text_gone(&page, "What should smelt work on?", Duration::from_secs(15)).await, "the start screen goes");
    assert!(page.evaluate("!document.querySelector('#persona-set')").await.expect("read").into_value::<bool>().expect("bool"));
}

/// B2: at a phone's 360px, and at 721px with the widest name and a long
/// description, neither the picker nor the page scrolls sideways.
pub(super) async fn scenario_persona_picker_fits_narrow_layouts(t: &Scenario<'_>) {
    let dir = unique_id("b2wide");
    let name = "a".repeat(64);
    // The longest description a persona may have: 1,024 characters.
    let description: String = "A long description. ".repeat(52).chars().take(1024).collect();
    let id = conversation_with_personas(t, &dir, &[(name.as_str(), persona_md(&name, &description, ""))]).await;
    for (width, height, mobile) in [(360, 780, true), (721, 900, false)] {
        let page = t.tab("about:blank").await;
        page.execute(chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams::new(width, height, 2.0, mobile))
            .await
            .expect("set the window's size");
        page.goto(t.url(&format!("conversation/{id}"))).await.expect("open the conversation");
        wait_for_live_client(&page, id).await;
        wait_for_element(&page, "#persona-picker", Duration::from_secs(15)).await;
        pick(&page, &format!("{dir}:{name}")).await;
        wait_for_element(&page, ".persona-picker-description", Duration::from_secs(5)).await;
        let measured: Vec<String> = page
            .evaluate(
                "(() => { const p = document.querySelector('.persona-picker'); \
                 return [String(p.scrollWidth - p.clientWidth), \
                 String(document.documentElement.scrollWidth - document.documentElement.clientWidth)]; })()",
            )
            .await
            .expect("measure the picker")
            .into_value()
            .expect("strings");
        assert!(measured[0].parse::<f64>().unwrap_or(1.0) <= 0.0, "{width}px: the picker overflows: {measured:?}");
        assert!(measured[1].parse::<f64>().unwrap_or(1.0) <= 0.0, "{width}px: the page scrolls sideways: {measured:?}");
        page.close().await.expect("close the tab");
    }
}

/// B3: a refused set (the conversation's provider doesn't list the
/// persona's model) shows its text in the picker, and nothing is set.
pub(super) async fn scenario_persona_picker_shows_a_refusal(t: &Scenario<'_>) {
    let dir = unique_id("b3");
    let opus = persona_md("driver", "Drives.", "smelt:\n  model: sme481-model-no-provider-lists\n");
    let id = conversation_with_personas(t, &dir, &[("driver", opus)]).await;
    let page = t.tab(t.url(&format!("conversation/{id}"))).await;
    wait_for_live_client(&page, id).await;
    wait_for_element(&page, "#persona-picker", Duration::from_secs(15)).await;
    pick(&page, &format!("{dir}:driver")).await;
    click_when_present(&page, "#persona-picker-set", Duration::from_secs(5)).await;
    assert!(
        wait_for_text(&page, "asks for model `sme481-model-no-provider-lists`", Duration::from_secs(15)).await,
        "the refusal shows"
    );
    assert!(page.evaluate("!!document.querySelector('#persona-picker')").await.expect("read").into_value::<bool>().expect("bool"));
    assert!(db::get_top_level_persona(t.pool, id).await.expect("read").is_none(), "nothing was set");
}
