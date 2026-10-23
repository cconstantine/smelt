//! The banner over a conversation's chat naming the agents under it that
//! wait for the user (SME-246): a question or a trust card in any of them,
//! however deep, is a click away from the parent the user is looking at.

use super::*;
use crate::api::agents::{AgentWaiting, get_agents_waiting};

/// Nothing at all while no agent under the open conversation waits: no
/// row, no gap. Refetched when a conversation or a question changes,
/// which covers a card appearing or going (`git::publish_repos`).
#[component]
pub(super) fn AgentsWaitingBanner(
    selected: Memo<Option<i64>>,
    conversations_changed: Signal<u64>,
    questions_changed: Signal<u64>,
) -> Element {
    // Tagged with its conversation, as the transcript's load is: a
    // restarted resource keeps the last conversation's list until the new
    // one lands, and that list mustn't show over this one.
    let waiting = use_resource(move || {
        let id = selected();
        let _ = conversations_changed();
        let _ = questions_changed();
        async move {
            match id {
                Some(id) => Some((id, get_agents_waiting(id).await)),
                None => None,
            }
        }
    });
    let agents: Vec<AgentWaiting> = match &*waiting.read() {
        Some(Some((id, Ok(agents)))) if selected() == Some(*id) => agents.clone(),
        _ => Vec::new(),
    };
    if agents.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "agents-waiting-banner", role: "status",
            span { class: "agents-waiting-label", {banner_label(agents.len())} }
            for agent in agents {
                Link {
                    key: "{agent.id}",
                    class: "agents-waiting-link",
                    to: Route::ConversationRoute { id: agent.id },
                    "{agent.title}"
                }
            }
        }
    }
}

/// What the banner says before its links.
fn banner_label(count: usize) -> String {
    match count {
        1 => "An agent under this conversation is waiting for you:".to_string(),
        n => format!("{n} agents under this conversation are waiting for you:"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_banner_says_how_many_agents_wait() {
        assert_eq!(banner_label(1), "An agent under this conversation is waiting for you:");
        assert_eq!(banner_label(3), "3 agents under this conversation are waiting for you:");
    }
}
