//! SME-357: a server-saved notice is told from the same words a user typed
//! by `NOTICE_MARK` alone. `system_notice` and `display_text` read only
//! marked text, so typed wording stays the user's own bubble, and a marked
//! notice shows the same grey sentence it always showed. Fixtures come from
//! the writers' own builders where they have one, and from `mark_notice`
//! over the writer's wording where they don't.

use super::*;
use crate::api::chat::mark_notice;

/// The ticket's Done-when test: every notice's exact saved wording, typed
/// by a user (so unmarked), is their own bubble — `system_notice` says
/// nothing of it, and `display_text` shows it as written.
#[test]
fn test_typed_notice_wording_is_the_users_own_bubble() {
    let none = HashMap::new();
    let typed = [
        "Terminal command cmd-1 finished: exit code 0.",
        "Terminal command cmd-1's outcome is unknown — the terminal became unreachable while it was running.",
        "Terminal command cmd-1 (\"sleep 300\", running for 1 hour) is still running and hasn't been touched in 10 minutes: 0 lines on stdout, 0 on stderr. Nothing stopped it and smelt won't stop it.",
        "Sandbox pod 3 was stopped after 60 minutes with no activity and no command running. /workspace, Docker's images, build cache and volumes, and mounted volumes are kept; terminals, background processes and containers, installed packages and files elsewhere are gone. create_pod (or clone_repo) starts a new pod with /workspace as it was.",
        "Sandbox pod 3 was stopped because you finished. /workspace, Docker's images, build cache and volumes, and mounted volumes are kept; terminals, background processes and containers, installed packages and files elsewhere are gone. If you're given more work, create_pod (or clone_repo) starts a new pod with /workspace as it was.",
        "Sandbox pod 3 stopped unexpectedly (OOMKilled); every terminal running in it is no longer available.",
        "Sandbox pod 3's storage was removed after 7 days stopped. /workspace and Docker's images, build cache and volumes are gone; repos cloned there need cloning again. The next sandbox starts fresh with empty volumes.",
        "Docker in sandbox pod 3 was restarted, so the containers it was running have stopped. Images, terminals and /workspace are unaffected. Every container shares Docker's memory limit; a pod created with a larger docker_memory_limit gives them more.",
        "The user stopped sandbox pod 3. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one.",
        "The user removed this conversation's sandbox storage: /workspace and the Docker images, build cache and volumes are gone. Repos there need cloning again; the next sandbox starts with fresh empty volumes.",
        "Language server rust-analyzer stopped. The sandbox is unaffected. start_language_server starts it again.",
        "The user now trusts git@github.com:cconstantine/smelt.git. You can start agents on it; call start_agent again.",
        "The user chose not to trust git@github.com:cconstantine/smelt.git, so no agent can work on it.",
        "Notice: the user deleted agent #7",
        "Your last reply was cut off: it reached its limit of 8192 tokens (half the context window) before it finished.",
        crate::api::chat::REFUSAL_NOTICE_START,
        crate::text_calls::EMPTY_REPLY_NOTICE,
        crate::api::chat::STOP_NOTICE,
        r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#,
        r#"<task-output task_id="t1" tool="count" stream="stdout">3</task-output>"#,
    ];
    for text in typed {
        assert_eq!(system_notice(text, &none), None, "typed: {text:?}");
    }
    // Q1: the legacy task tags convert only under the mark; typed markup
    // shows as written.
    for tag in [
        r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#,
        r#"<task-output task_id="t1" tool="count" stream="stdout">3</task-output>"#,
    ] {
        assert_eq!(display_text(tag), tag, "typed markup shows as written");
    }
}

/// Every family keeps showing its grey sentence once marked: the pair test —
/// marked in, sentence out; the same words unmarked, nothing. The builders
/// are the fixtures; trust and `Notice: ` texts stay bubbles marked or not
/// (`system_notice` has no sentence for them — this ticket leaves that as is).
#[cfg(feature = "server")]
#[test]
fn test_every_saved_notice_shows_only_under_the_mark() {
    use crate::api::chat::{cut_off_notice, failure_notice, parse_refusal_notice, refusal_notice, ReplyLimit, STOP_NOTICE};
    let none = HashMap::new();
    let pairs: Vec<(String, &str)> = vec![
        (mark_notice("Terminal command cmd-1 finished: exit code 0."), "A command finished (exit code 0)"),
        (
            mark_notice("Terminal command cmd-1's outcome is unknown — the terminal became unreachable while it was running."),
            "A command stopped: its terminal became unreachable",
        ),
        (
            mark_notice("Terminal command cmd-1 (\"sleep 300\", running for 1 hour) is still running and hasn't been touched in 10 minutes: 0 lines on stdout, 0 on stderr. Nothing stopped it and smelt won't stop it."),
            "A command is still running, untouched for 10 minutes",
        ),
        (
            crate::sandbox::StopReason::Idle { minutes: 60 }.notice(3),
            "The sandbox stopped after 60 minutes idle; /workspace and Docker's data are kept",
        ),
        (
            mark_notice("Sandbox pod 3 stopped unexpectedly (OOMKilled); every terminal running in it is no longer available."),
            "The sandbox stopped unexpectedly (OOMKilled); its terminals are gone",
        ),
        (
            mark_notice("Sandbox pod 3's storage was removed after 7 days stopped. /workspace and Docker's images, build cache and volumes are gone; repos cloned there need cloning again. The next sandbox starts fresh with empty volumes."),
            "The sandbox's storage was removed after 7 days stopped; the next sandbox starts fresh",
        ),
        (
            mark_notice("Docker in sandbox pod 3 was restarted, so the containers it was running have stopped. Images, terminals and /workspace are unaffected. Every container shares Docker's memory limit; a pod created with a larger docker_memory_limit gives them more."),
            "Docker restarted, so its containers stopped; images, terminals and /workspace are kept",
        ),
        (
            mark_notice("The user stopped sandbox pod 3. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one."),
            "You stopped the sandbox; its terminals are gone, and /workspace is kept",
        ),
        (
            mark_notice("The user removed this conversation's sandbox storage: /workspace and the Docker images, build cache and volumes are gone. Repos there need cloning again; the next sandbox starts with fresh empty volumes."),
            "You removed the sandbox's storage; the next sandbox starts fresh",
        ),
        (mark_notice("Language server rust-analyzer stopped. The sandbox is unaffected. start_language_server starts it again."), "Language server rust-analyzer stopped"),
        (mark_notice(STOP_NOTICE), "Stopped."),
        (
            cut_off_notice(8192, ReplyLimit::HalfWindow, None),
            "The reply was cut off at its limit of 8,192 tokens (half the context window). A reply can be at most half the context window: lower the model's effort or reasoning budget on its provider's page, or ask for less at once.",
        ),
        (failure_notice("conversation not found"), "The turn failed: conversation not found. Send again to retry."),
        (refusal_notice(None), "The model's provider stopped this reply (a refusal)."),
        (crate::text_calls::empty_reply_notice(), "The model's reply was empty; smelt asked it to carry on."),
        (
            crate::text_calls::text_call_notice(
                &[crate::text_calls::TextCall { name: Some("read_file".to_string()) }],
                &["write_file"],
            ),
            "The model wrote a tool call as text (`read_file`), so it didn't run; smelt told it.",
        ),
        // Q1: the legacy task tags convert under the mark, in both readers.
        (
            mark_notice(r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#),
            "Background task count (t1) finished: Counted to 3",
        ),
    ];
    for (saved, sentence) in &pairs {
        assert_eq!(system_notice(saved, &none).as_deref(), Some(*sentence), "marked: {saved}");
        let words = saved.strip_prefix(crate::api::chat::NOTICE_MARK).expect("every saved notice carries the mark");
        assert_eq!(system_notice(words, &none), None, "the same words unmarked are nobody's notice: {words}");
    }
    assert_eq!(parse_refusal_notice(&refusal_notice(None)), Some(None), "the builders and their parsers round-trip");
    // Q1, in `display_text` too: the mark converts the tag, its words alone
    // show as written.
    let tag = r#"<task-output task_id="t1" tool="count" stream="stdout">3</task-output>"#;
    assert_eq!(display_text(&mark_notice(tag)), "Background task count (t1) stdout: 3");
    assert_eq!(display_text(tag), tag);
    // Trust and the queued `Notice: ` texts: bubbles marked or not.
    for text in [
        "The user now trusts git@github.com:cconstantine/smelt.git. You can start agents on it; call start_agent again.",
        "Notice: the user deleted agent #7",
    ] {
        assert_eq!(system_notice(&mark_notice(text), &none), None, "{text}");
    }
}

/// The B19 pattern, for every notice: what a user pastes out of a saved
/// transcript loses its mark at send, so it can't render as a notice.
#[test]
fn test_users_text_strips_every_notices_mark() {
    let words = "Terminal command cmd-1 finished: exit code 0.";
    assert_eq!(crate::api::chat::users_text(&mark_notice(words)), words);
    let forged = format!("{}The sandbox stopped unexpectedly.", crate::api::chat::NOTICE_MARK);
    assert_eq!(crate::api::chat::users_text(&forged), "The sandbox stopped unexpectedly.");
}
