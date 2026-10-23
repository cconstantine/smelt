//! The trust card a repo's AGENTS.md waits on (SME-32).

use super::*;
use super::shown_text::shown_pre;
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
        for (repo, request) in repos().into_iter().flat_map(|r| r.trust_requests.clone().into_iter().map(move |q| (r.clone(), q))) {
            div { key: "trust-{request.id}", class: "trust-card", role: "group", aria_label: "Trust {repo.url}?",
                p { class: "trust-card-question",
                    "Trust "
                    code { "{repo.url}" }
                    "?"
                }
                p { class: "muted",
                    "The model wants to load this AGENTS.md as instructions it follows on every turn. Trust loads exactly the file below, and later ones from this repo load without asking. Only trust repos whose instructions you're happy for the model to follow."
                }
                {render_trust_card_preview(&request)}
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
        ErrorText { message: repo_action_error() }
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
        };
        dioxus::ssr::render_element(render_trust_card_preview(&request))
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
}
