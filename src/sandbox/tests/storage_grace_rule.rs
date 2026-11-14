//! SME-521: the storage-grace rule and its settings, pure — no database,
//! no cluster. Every row of the plan's grace table has its test.

use chrono::NaiveDateTime;

use super::*;

const SETTINGS: GraceSettings = GraceSettings { default_days: 7, agent_days: 1, user_days: 30 };

fn at(day: u32) -> NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, day).expect("a date").and_hms_opt(12, 0, 0).expect("a time")
}

/// A conversation whose latest row ended on day `ended` of the month.
fn ended(ended: u32, stopped_by_user: Option<bool>, quiet: bool, agent_done: bool) -> GraceInput {
    GraceInput {
        latest: Some(LatestPod { pod_id: 1, terminated_at: Some(at(ended)), stopped_by_user, quiet }),
        agent_done,
    }
}

fn removes(at: NaiveDateTime, days: u32, class: GraceClass) -> StorageFate {
    StorageFate::RemovesAt { at, days, class }
}

#[test]
fn test_each_grace_setting_parses_like_the_first() {
    for (name, default) in [
        ("SANDBOX_STORAGE_REMOVAL_DAYS", 7),
        (AGENT_DAYS_NAME, DEFAULT_AGENT_STORAGE_REMOVAL_DAYS),
        (USER_DAYS_NAME, DEFAULT_USER_STOPPED_STORAGE_DAYS),
    ] {
        assert_eq!(days_from(name, None, default), default, "{name}: unset is the default");
        assert_eq!(days_from(name, Some(String::new()), default), default, "{name}: empty is unset");
        assert_eq!(days_from(name, Some("0".into()), default), 0, "{name}: 0 is kept, not a fallback");
        assert_eq!(days_from(name, Some("30".into()), default), 30, "{name}");
        assert_eq!(days_from(name, Some("365".into()), default), 365, "{name}: a year is the ceiling");
        for bad in ["366", "604800", "-1", "junk", "2.5", " 7 "] {
            assert_eq!(days_from(name, Some(bad.into()), default), default, "{name}: {bad:?} falls back");
        }
    }
    assert_eq!((DEFAULT_AGENT_STORAGE_REMOVAL_DAYS, DEFAULT_USER_STOPPED_STORAGE_DAYS), (1, 30), "the user's defaults (Q1, Q2)");
}

#[test]
fn test_a_live_latest_row_is_in_use_whatever_else_is_true() {
    let live = GraceInput {
        latest: Some(LatestPod { pod_id: 1, terminated_at: None, stopped_by_user: None, quiet: false }),
        agent_done: true,
    };
    assert_eq!(fate(&live, at(1), &SETTINGS), StorageFate::Live);
}

/// Q2: a done sub-agent gets the agent grace, even over the user's Stop.
#[test]
fn test_a_done_agent_gets_the_agent_grace_over_a_stop() {
    for stopped_by_user in [Some(true), Some(false), None] {
        assert_eq!(
            fate(&ended(10, stopped_by_user, false, true), at(1), &SETTINGS),
            removes(at(11), 1, GraceClass::Agent),
            "{stopped_by_user:?}"
        );
    }
}

#[test]
fn test_the_users_stop_gets_its_own_grace() {
    assert_eq!(fate(&ended(1, Some(true), false, false), at(1), &SETTINGS), removes(at(1) + chrono::Duration::days(30), 30, GraceClass::UserStop));
}

/// Every end that isn't a Stop — `terminate_pod`, a lost pod, a failed
/// create, an idle or finish stop — gets the default, from its end.
#[test]
fn test_any_other_end_gets_the_default_from_its_end() {
    assert_eq!(fate(&ended(3, Some(false), false, false), at(1), &SETTINGS), removes(at(10), 7, GraceClass::Default));
    assert_eq!(fate(&ended(3, Some(false), true, false), at(1), &SETTINGS), removes(at(10), 7, GraceClass::Default));
}

/// Q3: a row from before the column whose end can't be told gets the
/// Stop's grace, or the default when the Stop's is 0; a quiet mark tells
/// it was no Stop.
#[test]
fn test_an_end_from_before_the_column_gets_the_stops_grace() {
    let unknown = ended(1, None, false, false);
    assert_eq!(fate(&unknown, at(1), &SETTINGS), removes(at(1) + chrono::Duration::days(30), 30, GraceClass::Unknown));
    let no_stop_grace = GraceSettings { user_days: 0, ..SETTINGS };
    assert_eq!(fate(&unknown, at(1), &no_stop_grace), removes(at(8), 7, GraceClass::Unknown));
    assert_eq!(fate(&ended(1, None, true, false), at(1), &SETTINGS), removes(at(8), 7, GraceClass::Default));
}

/// Claims with no pod row run from the oldest claim's creation: the agent
/// grace for a done agent, else the default.
#[test]
fn test_claims_with_no_row_run_from_their_creation() {
    let none = GraceInput { latest: None, agent_done: false };
    assert_eq!(fate(&none, at(2), &SETTINGS), removes(at(9), 7, GraceClass::NoPod));
    let agent = GraceInput { latest: None, agent_done: true };
    assert_eq!(fate(&agent, at(2), &SETTINGS), removes(at(3), 1, GraceClass::Agent));
}

/// A class whose days are 0 is kept; a default of 0 is the off switch
/// for every class.
#[test]
fn test_zero_days_keep_and_a_zero_default_turns_it_all_off() {
    let keep_stops = GraceSettings { user_days: 0, ..SETTINGS };
    assert_eq!(fate(&ended(1, Some(true), false, false), at(1), &keep_stops), StorageFate::Kept);
    let keep_agents = GraceSettings { agent_days: 0, ..SETTINGS };
    assert_eq!(fate(&ended(1, None, false, true), at(1), &keep_agents), StorageFate::Kept);
    let off = GraceSettings { default_days: 0, ..SETTINGS };
    for input in [ended(1, Some(true), false, false), ended(1, None, false, true), GraceInput { latest: None, agent_done: false }] {
        assert_eq!(fate(&input, at(1), &off), StorageFate::Kept, "{input:?}");
    }
}

#[test]
fn test_the_notice_names_the_class_and_its_days() {
    assert!(removal_notice_text(5, 7, GraceClass::Default).contains("removed after 7 days stopped"));
    assert!(removal_notice_text(5, 30, GraceClass::UserStop).contains("removed after 30 days stopped"));
    assert!(removal_notice_text(5, 1, GraceClass::Agent).contains("removed after 1 day finished"));
}
