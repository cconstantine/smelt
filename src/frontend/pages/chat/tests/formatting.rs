use super::*;

#[test]
fn test_format_tool_input_pretty_prints_json_object() {
    let input = serde_json::json!({"a": 2, "b": 3});
    assert_eq!(format_tool_input(&input), "{\n  \"a\": 2,\n  \"b\": 3\n}");
}

#[test]
fn test_tool_result_label_distinguishes_error_from_success() {
    assert_eq!(tool_result_label(false), "Tool result");
    assert_eq!(tool_result_label(true), "Tool error");
}

#[test]
fn test_diff_lines_identical_content_is_all_equal() {
    let result = diff_lines("a\nb\nc\n", "a\nb\nc\n");
    assert_eq!(
        result,
        vec![
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "a".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "b".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "c".to_string()
            },
        ]
    );
}

#[test]
fn test_diff_lines_detects_a_changed_line_as_removed_plus_added() {
    let result = diff_lines("a\nb\nc\n", "a\nx\nc\n");
    assert_eq!(
        result,
        vec![
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "a".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Removed,
                content: "b".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Added,
                content: "x".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "c".to_string()
            },
        ]
    );
}

#[test]
fn test_diff_lines_keeps_common_context_around_a_multiline_change() {
    // Proves this is a real diff, not "all of old removed, all of new
    // added" — only the differing middle line should be marked.
    let old = "fn f() {\n    old_body();\n}\n";
    let new = "fn f() {\n    new_body();\n}\n";
    let result = diff_lines(old, new);
    assert_eq!(
        result,
        vec![
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "fn f() {".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Removed,
                content: "    old_body();".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Added,
                content: "    new_body();".to_string()
            },
            DiffLine {
                kind: DiffLineKind::Equal,
                content: "}".to_string()
            },
        ]
    );
}

fn tool_use_message(id: i64, tool_use_id: &str, name: &str) -> Message {
    Message {
        id,
        conversation_id: 1,
        role: "assistant".to_string(),
        content: serde_json::to_string(&[ContentBlock::ToolUse {
            id: tool_use_id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({}),
        }])
        .expect("ContentBlock always serializes"),
        created_at: chrono::Utc::now().naive_utc(),
    }
}

#[test]
fn test_tool_use_names_by_id_maps_every_tool_use_across_messages() {
    let messages = vec![
        test_message(1),
        tool_use_message(2, "call_1", "read_file"),
        tool_use_message(3, "call_2", "todoread"),
    ];
    let names = tool_use_names_by_id(&messages);
    assert_eq!(names.get("call_1").map(String::as_str), Some("read_file"));
    assert_eq!(names.get("call_2").map(String::as_str), Some("todoread"));
    assert_eq!(names.get("call_3"), None);
}

#[test]
fn test_format_timestamp_uses_12_hour_clock_with_am_pm() {
    let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(14, 32, 0)
        .unwrap();
    assert_eq!(format_timestamp(dt, 0), "2:32 PM");
}

#[test]
fn test_format_timestamp_midnight_and_noon() {
    let midnight = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(0, 5, 0)
        .unwrap();
    assert_eq!(format_timestamp(midnight, 0), "12:05 AM");

    let noon = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    assert_eq!(format_timestamp(noon, 0), "12:00 PM");
}

#[test]
fn test_format_timestamp_applies_negative_offset_for_a_timezone_behind_utc() {
    // US Eastern Standard Time is UTC-5: 2:32 PM UTC -> 9:32 AM local.
    let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(14, 32, 0)
        .unwrap();
    assert_eq!(format_timestamp(dt, -5 * 60), "9:32 AM");
}

#[test]
fn test_format_timestamp_applies_positive_offset_for_a_timezone_ahead_of_utc() {
    // Japan Standard Time is UTC+9: 2:32 PM UTC -> 11:32 PM local.
    let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(14, 32, 0)
        .unwrap();
    assert_eq!(format_timestamp(dt, 9 * 60), "11:32 PM");
}

#[test]
fn test_format_timestamp_offset_crosses_a_day_boundary() {
    // 11:32 PM UTC, timezone ahead by 2 hours -> 1:32 AM the next day.
    // format_timestamp only ever shows a time, so the day rollover
    // itself isn't asserted here, just that the hour wraps correctly.
    let dt = chrono::NaiveDate::from_ymd_opt(2026, 7, 28)
        .unwrap()
        .and_hms_opt(23, 32, 0)
        .unwrap();
    assert_eq!(format_timestamp(dt, 2 * 60), "1:32 AM");
}
