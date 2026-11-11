use super::*;

fn conversation(id: i64, parent_id: Option<i64>) -> ConversationNode {
    let at = chrono::NaiveDateTime::default();
    ConversationNode::from(crate::models::Conversation { id, title: format!("c{id}"), created_at: at, updated_at: at, parent_id })
}

fn agent(id: i64, parent_id: i64, persona: Option<&str>, status: &str) -> ConversationNode {
    let mut node = conversation(id, Some(parent_id));
    node.persona = persona.map(str::to_string);
    node.status = Some(status.to_string());
    node
}

#[test]
fn test_an_agents_meta_line_shows_its_status_then_its_persona() {
    let html = dioxus::ssr::render_element(agent_meta(&agent(2, 1, Some("smelt:developer"), "waiting for the user")));
    assert!(html.contains(r#"class="conversation-agent-meta""#), "{html}");
    assert!(html.contains(">smelt:developer<"), "{html}");
    assert!(html.contains(r#"class="agent-status status-waiting-for-the-user">waiting for the user<"#), "{html}");
    assert!(html.find("waiting for the user") < html.find("smelt:developer"), "the status comes first: {html}");
}

#[test]
fn test_a_parent_row_says_how_many_agents_it_has_and_how_many_work() {
    let parent = tree::build_tree(
        &[node_of(1, None, false), node_of(2, Some(1), true), node_of(3, Some(1), false)],
        &HashSet::new(),
    );
    let html = dioxus::ssr::render_element(tree_summary(&parent[0]));
    assert!(html.contains(">2 agents<") && html.contains(">1 working<"), "{html}");
    let idle = tree::build_tree(&[node_of(1, None, false), node_of(2, Some(1), false)], &HashSet::new());
    let html = dioxus::ssr::render_element(tree_summary(&idle[0]));
    assert!(html.contains(">1 agent<") && !html.contains("working"), "{html}");
    let html = dioxus::ssr::render_element(tree_summary(&idle[1]));
    assert!(html.is_empty(), "no agents, no line: {html}");
}

fn node_of(id: i64, parent_id: Option<i64>, busy: bool) -> ConversationNode {
    let mut node = agent(id, parent_id.unwrap_or(0), None, "running");
    node.parent_id = parent_id;
    node.status = None;
    node.busy = busy;
    node
}

#[test]
fn test_an_agent_without_a_persona_shows_its_status_alone() {
    let html = dioxus::ssr::render_element(agent_meta(&agent(2, 1, None, "idle")));
    assert!(html.contains(">idle<"), "{html}");
    assert!(!html.contains("agent-persona") && !html.contains("agent-meta-sep"), "{html}");
}

#[test]
fn test_the_users_own_conversation_has_no_meta_line() {
    assert_eq!(dioxus::ssr::render_element(agent_meta(&conversation(1, None))), "");
}

#[test]
fn test_the_waiting_count_names_how_many_and_is_absent_at_zero() {
    let label = |n: usize| format!("{n} waiting");
    assert_eq!(dioxus::ssr::render_element(waiting_count(0, label)), "");
    let two = dioxus::ssr::render_element(waiting_count(2, label));
    assert!(two.contains(r#"title="2 waiting""#) && two.contains(r#"aria-label="2 waiting""#), "{two}");
    assert!(two.contains(">?2<"), "{two}");
}

#[test]
fn test_the_waiting_total_counts_every_conversation_waiting() {
    let mut list = vec![conversation(1, None), agent(2, 1, None, "waiting for the user"), conversation(3, None)];
    list[1].waiting_for_user = true;
    list[2].waiting_for_user = true;
    assert_eq!(waiting_total(&list), 2);
}

#[test]
fn test_the_delete_label_counts_the_whole_subtree() {
    let list = vec![
        conversation(1, None),
        conversation(2, Some(1)),
        conversation(3, Some(1)),
        conversation(4, Some(2)),
        conversation(5, None),
    ];
    assert_eq!(delete_confirm_label(&list, 1), "Delete + 3 agents?");
    assert_eq!(delete_confirm_label(&list, 2), "Delete + 1 agent?");
    assert_eq!(delete_confirm_label(&list, 5), "Confirm?");
}
