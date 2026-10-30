//! The trust card a repo's AGENTS.md waits on (SME-32).

use super::*;
use super::shown_text::{hidden_text_notice, shown_pre};
use crate::git::TrustRequest;

/// One card per AGENTS.md waiting for the user's trust, and the last
/// Trust / Don't trust failure.
#[component]
pub(super) fn TrustCards(
    selected: Memo<Option<i64>>,
    state: Store<ConversationState>,
) -> Element {
    let repos = state.repos();
    let mut repo_action_error = state.repo_action_error();
    // Trust or Don't trust the AGENTS.md the user was shown (its hash).
    let mut decide = move |request_id: i64, shown_hash: String, trust: bool| {
        let Some(id) = selected() else { return };
        repo_action_error.set(None);
        spawn(async move {
            if let Err(e) = decide_repo_trust(id, request_id, shown_hash, trust).await
                // Not if the user has moved on to another conversation.
                && selected() == Some(id)
            {
                repo_action_error.set(Some(server_error_message(&e)));
            }
        });
    };
    rsx! {
        // A repo's AGENTS.md waits for the user's trust before it
        // becomes instructions the model follows (SME-32).
        // The question, its `aria_label` and, when the count is not zero,
        // the notice line (SME-283): the remote being granted shows
        // through the escape, and the decision it feeds stays the card's
        // `request.hash`, never this showing.
        for (repo, request) in repos().into_iter().flat_map(|r| r.trust_requests.clone().into_iter().map(move |q| (r.clone(), q))) {
            {
                let url_shown = question_url_shown(&repo.url);
                let hidden = crate::git::hidden_characters(&repo.url);
                rsx! {
                    div { key: "trust-{request.id}", class: "trust-card", role: "group", aria_label: "Trust {url_shown}?",
                        p { class: "trust-card-question",
                            "Trust "
                            code { "{url_shown}" }
                            "?"
                        }
                        if hidden > 0 {
                            {hidden_text_notice(hidden)}
                        }
                        {render_trust_card_body(&request)}
                        div { class: "trust-card-buttons",
                            button {
                                class: "trust-card-trust",
                                r#type: "button",
                                onclick: {
                                    let request_id = request.id;
                                    let shown_hash = request.hash.clone();
                                    move |_| decide(request_id, shown_hash.clone(), true)
                                },
                                "Trust"
                            }
                            button {
                                class: "trust-card-decline",
                                r#type: "button",
                                onclick: {
                                    let request_id = request.id;
                                    let shown_hash = request.hash.clone();
                                    move |_| decide(request_id, shown_hash.clone(), false)
                                },
                                "Don't trust"
                            }
                        }
                    }
                }
            }
        }
        ErrorText { message: repo_action_error() }
    }
}

/// The question line's remote (SME-283): a row recorded before the fix
/// can hold a url that is another's invisible twin, and this is the
/// moment of granting — it shows through SME-276's escape, like the
/// card's preview does.
fn question_url_shown(url: &str) -> String {
    crate::git::plain_multiline_text(url)
}

/// The card's body, by card kind (SME-279): a repo card (no file) says
/// agents are what's at stake and shows no preview; a file card keeps
/// its copy and its preview (SME-277).
fn render_trust_card_body(request: &TrustRequest) -> Element {
    if request.is_repo {
        rsx! {
            p { class: "muted",
                "The model wants to start an agent on this repo. Trust lets agents clone and work on {request.path}, and its AGENTS.md files load without asking. Only trust repos you're happy for the model to work on."
            }
        }
    } else {
        rsx! {
            p { class: "muted",
                "The model wants to load this AGENTS.md as instructions it follows on every turn. Trust loads exactly the file below, and later ones from this repo load without asking. Only trust repos whose instructions you're happy for the model to follow."
            }
            {render_trust_card_preview(request)}
        }
    }
}

/// The card's preview (SME-277): the file escaped for the reader, with the
/// count notice above it when needed, under the unchanged `<summary>` — a
/// path is `plain_path_text`-checked server-side (SME-271), so it cannot
/// need escaping here. The question line and both buttons stay in
/// `TrustCards`: the decision goes by `request.hash`, the sha256 of the
/// raw content (`git/server/instructions.rs`), never by this showing.
fn render_trust_card_preview(request: &TrustRequest) -> Element {
    rsx! {
        details { class: "trust-card-preview", open: true,
            summary { "{request.path}" }
            {shown_pre(&request.content, None)}
        }
    }
}

/// SME-277's SSR tests of the card's preview: smuggled tag characters
/// show escaped with their count and never raw, and the `<summary>`
/// keeps the plain path. What Trust loads is pinned by `request.hash`,
/// sent by the buttons in `TrustCards` above, not by this preview; that
/// flow stays covered server-side by `git::server::trust_race_tests`.
#[cfg(all(test, feature = "server"))]
mod preview_tests {
    use super::*;

    fn card(content: &str) -> String {
        let request = TrustRequest {
            id: 7,
            path: "/workspace/v/AGENTS.md".to_string(),
            content: content.to_string(),
            hash: "0".repeat(64),
            is_repo: false,
        };
        dioxus::ssr::render_element(render_trust_card_preview(&request))
    }

    fn body(is_repo: bool) -> String {
        let request = TrustRequest {
            id: 7,
            path: if is_repo { "/workspace/v".to_string() } else { "/workspace/v/AGENTS.md".to_string() },
            content: "x".to_string(),
            hash: "0".repeat(64),
            is_repo,
        };
        dioxus::ssr::render_element(render_trust_card_body(&request))
    }

    #[test]
    fn test_the_card_shows_hidden_characters_escaped_with_their_count() {
        let html = card("trust me\u{200b}\u{e006f}\u{e0062}\u{e0065}\u{e0079}");
        assert!(html.contains("\\u{200b}"), "{html}");
        assert!(html.contains("\\u{e006f}"), "{html}");
        assert!(html.contains("5 characters"), "{html}");
        for c in "\u{200b}\u{e006f}\u{e0062}\u{e0065}\u{e0079}".chars() {
            assert!(!html.contains(c), "raw U+{:X} in the card's html", c as u32);
        }
        assert!(html.contains("/workspace/v/AGENTS.md"), "{html}");
        assert!(html.contains("trust-card-preview"), "{html}");
    }

    #[test]
    fn test_the_card_with_nothing_hidden_shows_its_text_exactly() {
        let html = card("# House rules\nBe kind.\n日本 é\n");
        assert!(!html.contains("muted"), "{html}");
        assert!(html.contains("Be kind."), "{html}");
    }

    /// SME-279: a repo card (no file) says it's about agents and shows no
    /// preview; a file card's copy and preview stay as they were.
    #[test]
    fn test_the_repo_card_shows_repo_copy_and_no_preview() {
        let html = body(true);
        assert!(html.contains("start an agent"), "{html}");
        assert!(html.contains("/workspace/v"), "{html}");
        assert!(!html.contains("trust-card-preview"), "{html}");
        assert!(!html.contains("load this AGENTS.md"), "{html}");

        let html = body(false);
        assert!(html.contains("trust-card-preview"), "{html}");
        assert!(html.contains("load this AGENTS.md"), "{html}");
        assert!(!html.contains("start an agent"), "{html}");
    }

    /// SME-283: the question line names the remote the user is about to
    /// grant, and tag characters spelling "ob" inside it must reach it
    /// escaped, never raw; a plain url reads byte-identically (pin: the
    /// escape adds nothing where there is nothing to escape).
    #[test]
    fn test_the_trust_card_question_escapes_the_remote() {
        let shown = question_url_shown("git\u{e006f}\u{e0062}hub.com/o/r");
        assert!(shown.contains("\\u{e006f}") && shown.contains("\\u{e0062}"), "{shown}");
        for c in ['\u{e006f}', '\u{e0062}'] {
            assert!(!shown.contains(c), "raw U+{:X} in the question: {shown:?}", c as u32);
        }
        assert_eq!(question_url_shown("github.com/o/r"), "github.com/o/r");
    }
}
