//! A tree row's persona and status, one test per row of the design's
//! status table (agents.md, "Statuses"), through `node`.

use std::collections::HashSet;

use super::*;

fn row(id: i64, state: Option<&str>) -> crate::db::TreeRow {
    let at = NaiveDateTime::default();
    crate::db::TreeRow {
        id,
        title: format!("c{id}"),
        created_at: at,
        updated_at: at,
        parent_id: state.map(|_| 1),
        state: state.map(str::to_string),
        persona_name: None,
        persona_path: None,
        waiting_for_user: false,
        waiting_for_parent: false,
    }
}

fn status(row: crate::db::TreeRow, running: bool) -> Option<String> {
    let running = if running { HashSet::from([row.id]) } else { HashSet::new() };
    node(row, &running).status
}

#[test]
fn test_a_stored_lifecycle_that_ends_or_hasnt_begun_comes_first() {
    for (state, shown) in [("starting", "starting"), ("failed", "failed"), ("stopped", "stopped")] {
        let mut r = row(2, Some(state));
        r.waiting_for_user = true;
        r.waiting_for_parent = true;
        assert_eq!(status(r, true).as_deref(), Some(shown), "{state}");
    }
}

#[test]
fn test_a_turn_in_flight_is_running_before_any_wait() {
    let mut r = row(2, Some("active"));
    r.waiting_for_user = true;
    assert_eq!(status(r, true).as_deref(), Some("running"));
}

#[test]
fn test_a_question_or_card_for_the_user_is_waiting_for_the_user() {
    let mut r = row(2, Some("active"));
    r.waiting_for_user = true;
    r.waiting_for_parent = true;
    assert_eq!(status(r, false).as_deref(), Some("waiting for the user"));
}

#[test]
fn test_a_question_to_the_parent_is_waiting_for_its_parent() {
    let mut r = row(2, Some("active"));
    r.waiting_for_parent = true;
    assert_eq!(status(r, false).as_deref(), Some("waiting for its parent"));
}

#[test]
fn test_a_finished_agent_with_nothing_running_is_finished() {
    assert_eq!(status(row(2, Some("finished")), false).as_deref(), Some("finished"));
    assert_eq!(status(row(2, Some("finished")), true).as_deref(), Some("running"), "the user wrote to it again");
}

#[test]
fn test_an_active_agent_with_nothing_to_do_is_idle() {
    assert_eq!(status(row(2, Some("active")), false).as_deref(), Some("idle"));
}

#[test]
fn test_the_users_own_conversation_has_no_status_but_keeps_its_marks() {
    let mut r = row(1, None);
    r.waiting_for_user = true;
    let shown = node(r, &HashSet::from([1]));
    assert_eq!((shown.status, shown.persona), (None, None));
    assert!(shown.busy && shown.waiting_for_user);
}

#[test]
fn test_the_persona_is_dir_and_name_from_its_path_or_the_name_while_starting() {
    let mut started = row(2, Some("active"));
    started.persona_name = Some("developer".to_string());
    started.persona_path = Some("/workspace/smelt/.agents/personas/developer.md".to_string());
    assert_eq!(node(started, &HashSet::new()).persona.as_deref(), Some("smelt:developer"));
    let mut starting = row(3, Some("starting"));
    starting.persona_name = Some("planner".to_string());
    assert_eq!(node(starting, &HashSet::new()).persona.as_deref(), Some("planner"));
    assert_eq!(node(row(4, Some("active")), &HashSet::new()).persona, None);
}

#[test]
fn test_only_its_own_turn_makes_a_row_busy() {
    let shown = node(row(2, Some("active")), &HashSet::from([7]));
    assert!(!shown.busy);
    assert_eq!(shown.status.as_deref(), Some("idle"));
}
