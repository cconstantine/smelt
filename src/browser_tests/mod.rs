//! The one comprehensive browser test tier — started for `sandbox-visibility`
//! (see SME-10), extended for
//! `auto-compaction`'s context-usage indicator/detail view and compaction
//! divider (see SME-18) — per
//! `docs/testing.md`'s own note that this tier was "worth extending once
//! another feature has a similar need for real-DOM verification." Runs the
//! real app in-process (no `lib.rs` exists, so an external `tests/`
//! integration test couldn't reach `db`/`sandbox`/`anthropic::tools` at all)
//! against a real headless `chrome-headless-shell`, driven over CDP via
//! `chromiumoxide` (no `chromedriver` to download/manage). `#[ignore]`d by
//! default: needs `scripts/browser-check/setup.sh` run first, and a real
//! Postgres + k3s cluster reachable the same way every other real-cluster
//! test in this codebase already assumes.
//!
//! One `#[tokio::test]`, sharing one browser/server/`MANAGER` instance for
//! its whole duration — more than one `#[tokio::test]` here touching
//! `sandbox::init()`/`db::init()` would risk the same
//! `OnceLock`-across-separate-runtimes hazard `docs/testing.md` documents
//! for `PgPool`, the same reasoning `sandbox-terminal`'s own real-cluster
//! test already applied. Inside it, each scenario is its own `scenario_*`
//! function with its own conversations and tabs, run by `run_scenario`
//! under its own time limit: a failing or hung scenario ends alone, the
//! rest still run, and the test fails at the end listing every failure
//! (SME-59). `SMELT_BROWSER_SCENARIOS=name,name` runs just those.
//!
//! Scenarios seed state directly via `db`/`anthropic::tools`; where one
//! needs the model, the model is a slow local mock upstream — this tier
//! verifies the browser/live-event pipeline and DOM rendering, not
//! tool-selection or compaction-trigger *logic* (already covered by
//! `turn`'s own mock-upstream tests), and this test environment (like
//! CI) has no real Anthropic credentials to make a live call with anyway.

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::Browser;

use crate::{anthropic, db, sandbox};

/// What one scenario works with: the shared browser, server and database,
/// plus the conversations and tabs it makes. Every tab a scenario opens
/// through `tab` is closed after it, however it ended: every open smelt
/// tab holds one always-open event stream, and over plain HTTP/1.1 the
/// browser allows only 6 connections per host across all tabs, so tabs a
/// failed scenario left open would starve the ones after it.
struct Scenario<'a> {
    pool: &'a sqlx::PgPool,
    harness: &'a BrowserTestHarness,
    /// The lane's own Chrome (see `run_lanes`).
    browser: &'a Browser,
    /// The lane's own share of the mock: its model, and its replies' counts.
    mock: &'a MockUpstream,
    created: &'a std::sync::Mutex<Vec<i64>>,
    tabs: std::sync::Mutex<Vec<chromiumoxide::Page>>,
}

impl Scenario<'_> {
    /// `path` under the app's address.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.harness.base_url)
    }

    /// Opens `url` in a new tab, closed after the scenario.
    async fn tab(&self, url: impl Into<String>) -> chromiumoxide::Page {
        let url: String = url.into();
        let page = self
            .browser
            .new_page(url.as_str())
            .await
            .unwrap_or_else(|e| panic!("open {url}: {e}"));
        self.tabs.lock().expect("the tab list lock").push(page.clone());
        page
    }

    /// A new conversation on the mock model, removed at the end of the run.
    async fn conversation(&self) -> crate::models::Conversation {
        new_conversation(self.pool, self.created, &self.mock.model).await
    }

    /// Closes every tab the scenario opened. A tab it already closed itself
    /// just fails to close again.
    async fn close_tabs(&self) {
        let tabs = std::mem::take(&mut *self.tabs.lock().expect("the tab list lock"));
        for tab in tabs {
            let _ = tokio::time::timeout(Duration::from_secs(5), tab.close()).await;
        }
    }
}

/// One scenario's outcome: how long it took, or why it failed.
type ScenarioResult = (&'static str, Result<Duration, String>);

/// A scenario, boxed: the biggest scenarios' futures don't fit the test
/// thread's stack inline (SME-90 hit it when every scenario was one future).
type ScenarioFn = for<'a, 'b> fn(&'a Scenario<'b>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>;

/// Whether a scenario runs beside others (SME-341's speed pass), or alone,
/// after every lane has finished: one that reads what the whole app holds
/// (every provider, pod, secret or conversation row), where another lane's
/// would show up mid-scenario.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Runs {
    Beside,
    Alone,
}

struct Entry {
    name: &'static str,
    limit_secs: u64,
    runs: Runs,
    run: ScenarioFn,
}

fn entry(name: &'static str, limit_secs: u64, runs: Runs, run: ScenarioFn) -> Entry {
    Entry { name, limit_secs, runs, run }
}

/// Runs one scenario under its own time limit and `catch_unwind`, so a
/// failure or a hang ends that scenario alone, and its tabs are closed
/// whatever happened.
async fn run_scenario(t: &Scenario<'_>, results: &std::sync::Mutex<Vec<ScenarioResult>>, e: &Entry) {
    let (name, limit_secs) = (e.name, e.limit_secs);
    eprintln!("browser tier: {name} ...");
    let started = tokio::time::Instant::now();
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(tokio::time::timeout(
        Duration::from_secs(limit_secs),
        (e.run)(t),
    )))
    .await;
    t.close_tabs().await;
    let result = match outcome {
        Ok(Ok(())) => Ok(started.elapsed()),
        Ok(Err(_)) => Err(format!("didn't finish within {limit_secs} s")),
        Err(panic) => Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panicked".to_string())),
    };
    match &result {
        Ok(took) => eprintln!("browser tier: {name} ok ({:.1} s)", took.as_secs_f64()),
        Err(why) => eprintln!("browser tier: {name} FAILED: {why}"),
    }
    results.lock().expect("the results lock").push((name, result));
}

/// Runs `entries`: those that run beside others across every lane, each
/// lane taking the next from one queue (longest limit first, so the long
/// ones don't start last), then those that run alone, one at a time, once
/// the lanes are done.
async fn run_lanes(lanes: &[Scenario<'_>], entries: Vec<&Entry>) -> Vec<ScenarioResult> {
    let results = std::sync::Mutex::new(Vec::new());
    let (mut beside, alone): (Vec<&Entry>, Vec<&Entry>) = entries.into_iter().partition(|e| e.runs == Runs::Beside);
    beside.sort_by_key(|e| std::cmp::Reverse(e.limit_secs));
    let queue = std::sync::Mutex::new(std::collections::VecDeque::from(beside));
    let results_ref = &results;
    let queue_ref = &queue;
    futures_util::future::join_all(lanes.iter().map(|t| async move {
        loop {
            let next = queue_ref.lock().expect("the queue lock").pop_front();
            let Some(e) = next else { break };
            run_scenario(t, results_ref, e).await;
        }
    }))
    .await;
    for e in alone {
        run_scenario(&lanes[0], &results, e).await;
    }
    results.into_inner().expect("the results lock")
}

/// Every scenario: its name (for `SMELT_BROWSER_SCENARIOS` and the report),
/// its time limit, and whether it can run beside others (`Runs`).
fn scenarios() -> Vec<Entry> {
    vec![
        entry("terminals", 120, Runs::Beside, |t| Box::pin(scenario_terminals(t))),
        entry("context_usage", 60, Runs::Beside, |t| Box::pin(scenario_context_usage(t))),
        entry("compaction_divider", 60, Runs::Beside, |t| Box::pin(scenario_compaction_divider(t))),
        entry("todo_panel", 60, Runs::Beside, |t| Box::pin(scenario_todo_panel(t))),
        entry("reply_stays_in_its_conversation", 60, Runs::Beside, |t| Box::pin(scenario_reply_stays_in_its_conversation(t))),
        entry("unrequested_reply", 60, Runs::Beside, |t| Box::pin(scenario_unrequested_reply(t))),
        entry("missing_conversation", 30, Runs::Beside, |t| Box::pin(scenario_missing_conversation(t))),
        entry("notification_failure_stays", 60, Runs::Beside, |t| Box::pin(scenario_notification_failure_stays(t))),
        entry("pods", 120, Runs::Alone, |t| Box::pin(scenario_pods(t))),
        entry("stop_a_turn", 60, Runs::Beside, |t| Box::pin(scenario_stop_a_turn(t))),
        entry("reply_on_conversation_stream", 60, Runs::Beside, |t| Box::pin(scenario_reply_on_conversation_stream(t))),
        entry("busy_session_layout", 120, Runs::Beside, |t| Box::pin(scenario_busy_session_layout(t))),
        entry("sidebar", 30, Runs::Alone, |t| Box::pin(scenario_sidebar(t))),
        // 90s, not 60: two Home tabs hydrate, then a 30s wait for the new row and
        // 10s for each disappearance. All of them return early — the hop is about a
        // second on a quiet pod — but a limit that fails a working run reads as
        // infrastructure (the same reasoning as switch_shows_no_start_screen's).
        entry("conversations_appear_live", 90, Runs::Alone, |t| Box::pin(scenario_conversations_appear_live(t))),
        entry("phone_width", 30, Runs::Beside, |t| Box::pin(scenario_phone_width(t))),
        entry("not_found", 30, Runs::Beside, |t| Box::pin(scenario_not_found(t))),
        entry("tool_rows", 30, Runs::Beside, |t| Box::pin(scenario_tool_rows(t))),
        entry("dark_mode", 30, Runs::Beside, |t| Box::pin(scenario_dark_mode(t))),
        entry("forms", 30, Runs::Alone, |t| Box::pin(scenario_forms(t))),
        entry("empty_conversation", 30, Runs::Beside, |t| Box::pin(scenario_empty_conversation(t))),
        entry("sandbox_dev_server", 300, Runs::Beside, |t| Box::pin(scenario_sandbox_dev_server(t))),
        entry("agents_md_trust", 180, Runs::Beside, |t| Box::pin(scenario_agents_md_trust(t))),
        // 90s, not 60: one tab hydrates, then the card's five waits and the
        // Trust hop. Every wait returns early on a working run — the bound is
        // only so a broken one reads as itself, not as infrastructure.
        entry("repo_trust_card", 90, Runs::Beside, |t| Box::pin(scenario_repo_trust_card(t))),
        entry("switch_resets_context_detail", 30, Runs::Beside, |t| Box::pin(scenario_switch_resets_context_detail(t))),
        entry("model_picker", 60, Runs::Alone, |t| Box::pin(scenario_model_picker(t))),
        entry("oauth_headers", 30, Runs::Alone, |t| Box::pin(scenario_oauth_headers(t))),
        entry("oauth_sign_in_expired", 30, Runs::Alone, |t| Box::pin(scenario_oauth_sign_in_expired(t))),
        entry("stale_bundle", 60, Runs::Alone, |t| Box::pin(scenario_stale_bundle(t))),
        entry("transcript_scroll", 60, Runs::Beside, |t| Box::pin(scenario_transcript_scroll(t))),
        // Alone: it clicks conversation rows over and over, at phone width
        // too, and other lanes' conversations arriving in the same sidebar
        // moved its row out from under the click ("covered by P", SME-341).
        entry("terminal_follow_after_switch", 180, Runs::Alone, |t| Box::pin(scenario_terminal_follow_after_switch(t))),
        entry("context_from_the_keyboard", 60, Runs::Beside, |t| Box::pin(scenario_context_from_the_keyboard(t))),
        entry("address_bar_after_session_closes", 90, Runs::Beside, |t| Box::pin(scenario_address_bar_after_session_closes(t))),
        entry("error_text", 60, Runs::Beside, |t| Box::pin(scenario_error_text(t))),
        entry("settings_two_step", 60, Runs::Alone, |t| Box::pin(scenario_settings_two_step(t))),
        entry("pointer_keeps_text_still", 120, Runs::Beside, |t| Box::pin(scenario_pointer_keeps_text_still(t))),
        entry("ask_user_card", 60, Runs::Beside, |t| Box::pin(scenario_ask_user_card(t))),
        entry("markdown_reply", 60, Runs::Beside, |t| Box::pin(scenario_markdown_reply(t))),
        entry("markdown_streaming", 60, Runs::Beside, |t| Box::pin(scenario_markdown_streaming(t))),
        entry("markdown_deep_code", 240, Runs::Beside, |t| Box::pin(scenario_markdown_deep_code(t))),
        entry("copy_reply", 120, Runs::Beside, |t| Box::pin(scenario_copy_reply(t))),
        entry("markdown_late_image", 90, Runs::Beside, |t| Box::pin(scenario_markdown_late_image(t))),
        entry("markdown_long_reply", 90, Runs::Beside, |t| Box::pin(scenario_markdown_long_reply(t))),
        entry("streaming_into_a_long_transcript", 120, Runs::Beside, |t| Box::pin(scenario_streaming_into_a_long_transcript(t))),
        entry("unreadable_message", 30, Runs::Beside, |t| Box::pin(scenario_unreadable_message(t))),
        entry("cut_off_notice", 30, Runs::Alone, |t| Box::pin(scenario_cut_off_notice(t))),
        entry("cut_off_in_a_call", 30, Runs::Alone, |t| Box::pin(scenario_cut_off_in_a_call(t))),
        entry("llama_cpp_provider", 60, Runs::Alone, |t| Box::pin(scenario_llama_cpp_provider(t))),
        entry("chat_errors_are_alerts", 30, Runs::Beside, |t| Box::pin(scenario_chat_errors_are_alerts(t))),
        entry("message_box_label", 30, Runs::Beside, |t| Box::pin(scenario_message_box_label(t))),
        entry("conversation_rows_by_keyboard", 60, Runs::Alone, |t| Box::pin(scenario_conversation_rows_by_keyboard(t))),
        entry("markdown_bare_urls", 60, Runs::Beside, |t| Box::pin(scenario_markdown_bare_urls(t))),
        entry("markdown_bare_urls_streaming", 60, Runs::Beside, |t| Box::pin(scenario_markdown_bare_urls_streaming(t))),
        entry("markdown_link_bidi_isolated", 60, Runs::Beside, |t| Box::pin(scenario_markdown_link_bidi_isolated(t))),
        entry("terminal_command_indicator", 180, Runs::Beside, |t| Box::pin(scenario_terminal_command_indicator(t))),
        entry("turn_error_clears_on_a_later_turn", 30, Runs::Beside, |t| Box::pin(scenario_turn_error_clears_on_a_later_turn(t))),
        entry("deleted_in_another_tab", 60, Runs::Beside, |t| Box::pin(scenario_deleted_in_another_tab(t))),
        entry("working_line_timer_survives_a_reload", 60, Runs::Beside, |t| Box::pin(scenario_working_line_timer_survives_a_reload(t))),
        entry("oauth_callback_error_link", 60, Runs::Alone, |t| Box::pin(scenario_oauth_callback_error_link(t))),
        // 60s, not 30: the scenario creates three conversations, hydrates, then
        // waits on five of its own 10s timeouts across four switches. They return
        // early when things are fast, but a slow run legitimately spends most of
        // that passing every step, so 30s can fail a working run and have it read
        // as an infrastructure failure (code review 2).
        entry("switch_shows_no_start_screen", 60, Runs::Beside, |t| Box::pin(scenario_switch_shows_no_start_screen(t))),
        entry("attach_error_outlives_a_switch", 60, Runs::Beside, |t| Box::pin(scenario_attach_error_outlives_a_switch(t))),
        entry("attach_error_after_the_first_message", 60, Runs::Beside, |t| Box::pin(scenario_attach_error_after_the_first_message(t))),
        entry("delete_names_its_agents", 60, Runs::Alone, |t| Box::pin(scenario_delete_names_its_agents(t))),
        entry("agent_tree", 90, Runs::Alone, |t| Box::pin(scenario_agent_tree(t))),
        entry("text_call_correction", 30, Runs::Alone, |t| Box::pin(scenario_text_call_correction(t))),
        entry("secrets_page", 60, Runs::Alone, |t| Box::pin(scenario_secrets_page(t))),
    ]
}

/// `SMELT_BROWSER_LANES`: how many scenarios run side by side, each lane
/// with its own Chrome and its own share of the mock. 3 by default, the
/// browser job's reserved cores; 1 runs them all one at a time, in the
/// order `scenarios` lists them.
fn lane_count() -> usize {
    std::env::var("SMELT_BROWSER_LANES").ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n >= 1).unwrap_or(3)
}

/// `SMELT_BROWSER_SCENARIOS`: a comma-separated list of scenario names to
/// run instead of all of them. Set-but-empty is unset, as everywhere in
/// smelt.
fn selected_scenarios() -> Option<Vec<String>> {
    let names = std::env::var("SMELT_BROWSER_SCENARIOS").ok()?;
    let names: Vec<String> = names
        .split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    (!names.is_empty()).then_some(names)
}

#[tokio::test]
#[ignore]
async fn test_end_to_end_browser_scenarios() {
    // Before db::init(): it would migrate whatever DATABASE_URL names.
    crate::test_lock::require();
    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("migrations should apply");
    // Pods and claims are named after conversation, pod and volume ids,
    // and the scratch database would count them from 1 like every
    // real-cluster unit test's, in the same namespace: a pod another run
    // was still stopping held up this run's first sandbox (SME-99).
    db::test_support::start_ids_clear_of_other_runs(pool)
        .await
        .expect("start the ids clear of other runs");
    sandbox::init().await.expect("sandbox manager");

    let harness = BrowserTestHarness::start().await;
    // The model every conversation the test creates runs on: a provider of
    // the test's own, so the dev database's providers and default are left
    // alone (SME-72). Removed with the conversations at the end. One
    // provider, with a model per lane.
    let lane_count = lane_count();
    let models: Vec<String> =
        (0..lane_count).map(|i| if i == 0 { MOCK_MODEL.to_string() } else { format!("{MOCK_MODEL}-lane-{i}") }).collect();
    let mocks = MockUpstream::start(&models).await;
    let mock_provider = db::create_inference_provider(
        pool,
        &format!("browser tier mock {}", std::process::id()),
        "anthropic",
        &format!("http://{}", mocks[0].addr),
        "api_key",
        "test-key",
        false,
        None,
        true,
        None,
    )
    .await
    .expect("save the mock provider");
    MOCK_PROVIDER.set(mock_provider.id).expect("the mock provider is set once");
    let mut browsers = Vec::new();
    for _ in 1..lane_count {
        browsers.push(harness.another_browser().await);
    }
    // Every conversation a scenario creates, so they (and their sandbox
    // pods) can be removed afterwards — this runs against a real database
    // and cluster, so leftovers show up in the app's own sidebar and pile
    // up pods until new ones stop starting.
    let created = std::sync::Mutex::new(Vec::new());
    let lanes: Vec<Scenario> = (0..lane_count)
        .map(|i| Scenario {
            pool,
            harness: &harness,
            browser: if i == 0 { &harness.browser } else { &browsers[i - 1] },
            mock: &mocks[i],
            created: &created,
            tabs: std::sync::Mutex::new(Vec::new()),
        })
        .collect();

    let only = selected_scenarios();
    let only = only.as_deref();
    let all = scenarios();
    let entries: Vec<&Entry> = all.iter().filter(|e| only.is_none_or(|only| only.iter().any(|n| n == e.name))).collect();
    let results = if lane_count == 1 {
        let results = std::sync::Mutex::new(Vec::new());
        for e in entries {
            run_scenario(&lanes[0], &results, e).await;
        }
        results.into_inner().expect("the results lock")
    } else {
        run_lanes(&lanes, entries).await
    };
    let known: Vec<&'static str> = all.iter().map(|e| e.name).collect();

    let mut failures: Vec<String> = results
        .iter()
        .filter_map(|(name, result)| result.as_ref().err().map(|why| format!("{name}: {why}")))
        .collect();
    for name in only.unwrap_or_default() {
        if !known.iter().any(|k| *k == name.as_str()) {
            failures.push(format!("{name}: no such scenario (SMELT_BROWSER_SCENARIOS)"));
        }
    }

    // Cleanup and the leftover check both run before `harness.shutdown()`.
    // The server's handlers run on dioxus's own worker runtimes, and
    // database and cluster connections they opened stay tied to those
    // runtimes. Once the harness shuts down, using one either fails ("A
    // Tokio 1.x context was found, but it is being shutdown", "runtime
    // dropped the dispatch task") or hangs forever (SME-39).
    drop(lanes);
    for mut browser in browsers {
        let _ = browser.close().await;
    }
    let created = created.into_inner().expect("the conversation list lock");
    let pod_ids = sandbox_pod_ids(pool, &created).await;
    remove_conversations(pool, &created).await;
    if let Err(e) = db::delete_inference_provider(pool, mock_provider.id).await {
        eprintln!("failed to delete the mock provider: {e}");
    }
    let mut leftovers = find_leftovers(pool, &created, &pod_ids).await;
    if let Ok(Some(_)) = db::get_inference_provider(pool, mock_provider.id).await {
        leftovers.push(format!("model provider {} in the database", mock_provider.id));
    }
    harness.shutdown().await;
    assert!(
        failures.is_empty(),
        "{} of {} browser scenarios failed:\n  {}",
        failures.len(),
        results.len(),
        failures.join("\n  ")
    );
    assert!(leftovers.is_empty(), "the test left things behind: {leftovers:?}");
}

mod harness;
mod mock;
mod basics;
mod layout;
mod pages;
mod dev_server;
mod settings;
mod terminal_follow;
mod terminal_scroll;
mod markdown;
mod copy_reply;
mod long_replies;
mod notices;
mod keyboard_and_errors;
mod links_and_commands;
mod repo_attach;
mod agents;
mod text_calls;

use harness::*;
use mock::*;
use basics::*;
use layout::*;
use pages::*;
use dev_server::*;
use settings::*;
use terminal_follow::*;
use terminal_scroll::*;
use markdown::*;
use copy_reply::*;
use long_replies::*;
use notices::*;
use keyboard_and_errors::*;
use links_and_commands::*;
use repo_attach::*;
use agents::*;
use text_calls::*;
