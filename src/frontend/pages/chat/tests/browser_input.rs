use super::*;

#[test]
fn test_browser_input_event_for_key_still_types_altgr_characters() {
    // Windows reports AltGr as Ctrl+Alt.
    assert_eq!(
        browser_input_event_for_key(
            keyboard_types::Key::Character("@".to_string()),
            keyboard_types::Modifiers::CONTROL | keyboard_types::Modifiers::ALT,
        ),
        Some(BrowserInputEvent::TypeText { text: "@".to_string() })
    );
}

#[test]
fn test_browser_input_event_for_key_types_shifted_characters() {
    assert_eq!(
        browser_input_event_for_key(
            keyboard_types::Key::Character("A".to_string()),
            keyboard_types::Modifiers::SHIFT,
        ),
        Some(BrowserInputEvent::TypeText { text: "A".to_string() })
    );
}

#[test]
fn test_browser_input_event_for_key_carries_modifiers_on_named_keys() {
    assert_eq!(
        browser_input_event_for_key(keyboard_types::Key::Tab, keyboard_types::Modifiers::SHIFT),
        Some(BrowserInputEvent::PressKey { key: "Tab".to_string(), modifiers: 8 })
    );
    assert_eq!(
        browser_input_event_for_key(
            keyboard_types::Key::Backspace,
            keyboard_types::Modifiers::CONTROL
        ),
        Some(BrowserInputEvent::PressKey { key: "Backspace".to_string(), modifiers: 2 })
    );
}

#[test]
fn test_cdp_modifiers_sets_one_bit_per_held_modifier() {
    use keyboard_types::Modifiers;
    assert_eq!(cdp_modifiers(Modifiers::empty()), 0);
    assert_eq!(cdp_modifiers(Modifiers::ALT), 1);
    assert_eq!(cdp_modifiers(Modifiers::CONTROL), 2);
    assert_eq!(cdp_modifiers(Modifiers::META), 4);
    assert_eq!(cdp_modifiers(Modifiers::SHIFT), 8);
    assert_eq!(cdp_modifiers(Modifiers::CONTROL | Modifiers::SHIFT), 10);
}

#[test]
fn test_a_reply_that_is_only_thinking_is_recognized() {
    let thinking = ContentBlock::Thinking {
        thinking: "The answer is /tmp and bar.".to_string(),
        signature: String::new(),
    };
    let text = ContentBlock::Text { text: "done".to_string() };
    assert!(reply_is_only_thinking("assistant", std::slice::from_ref(&thinking)));
    assert!(!reply_is_only_thinking("assistant", &[thinking.clone(), text]));
    assert!(!reply_is_only_thinking("user", std::slice::from_ref(&thinking)));
    assert!(!reply_is_only_thinking("assistant", &[]));
}

#[test]
fn test_display_text_reads_task_notices_as_sentences() {
    assert_eq!(
        display_text(r#"<task-notification task_id="lS9Y" tool="count">finished: Counted to 3</task-notification>"#),
        "Background task count (lS9Y) finished: Counted to 3"
    );
    assert_eq!(
        display_text(r#"<task-output task_id="lS9Y" tool="count" stream="stdout">count: 1/3</task-output>"#),
        "Background task count (lS9Y) stdout: count: 1/3"
    );
    assert_eq!(display_text("plain words <b>and a tag</b>"), "plain words <b>and a tag</b>");
}

/// SME-40 F2: the frame now scales to fit the panel, so a click on
/// the shrunk frame is scaled back up to the page's own pixels.
#[test]
fn test_frame_point_scales_a_shrunk_frame_back_to_page_pixels() {
    assert_eq!(frame_point(100.0, 50.0, 1280.0), (100.0, 50.0));
    assert_eq!(frame_point(100.0, 50.0, 640.0), (200.0, 100.0));
    // Before the first resize event arrives, don't scale by nonsense.
    assert_eq!(frame_point(100.0, 50.0, 0.0), (100.0, 50.0));
}

#[test]
fn test_wheel_delta_pixels_passes_pixels_through() {
    assert_eq!(wheel_delta_pixels(WheelDelta::pixels(0.0, 120.0, 0.0)), (0.0, 120.0));
}

#[test]
fn test_wheel_delta_pixels_converts_lines_and_pages() {
    assert_eq!(
        wheel_delta_pixels(WheelDelta::from_web_attributes(1, 0.0, 3.0, 0.0)),
        (0.0, 120.0)
    );
    assert_eq!(
        wheel_delta_pixels(WheelDelta::from_web_attributes(2, 1.0, -1.0, 0.0)),
        (1280.0, -800.0)
    );
}

#[test]
fn test_address_bar_value_follows_the_page_until_editing() {
    assert_eq!(address_bar_value(false, "typed", Some("https://a.example/")), "https://a.example/");
    assert_eq!(address_bar_value(false, "typed", None), "");
}

#[test]
fn test_address_bar_value_keeps_what_is_being_typed() {
    assert_eq!(address_bar_value(true, "exam", Some("https://a.example/")), "exam");
}

fn mv(x: f64) -> BrowserInputEvent {
    BrowserInputEvent::MouseMove { x, y: 0.0, left_held: false }
}

#[test]
fn test_coalesce_mouse_moves_keeps_only_the_last_of_a_run() {
    let down = BrowserInputEvent::MouseDown { x: 3.0, y: 0.0 };
    assert_eq!(
        coalesce_mouse_moves(vec![(1, mv(1.0)), (1, mv(2.0)), (1, down.clone()), (1, mv(4.0)), (1, mv(5.0))]),
        vec![(1, mv(2.0)), (1, down), (1, mv(5.0))]
    );
}

#[test]
fn test_coalesce_mouse_moves_keeps_moves_for_different_conversations() {
    assert_eq!(
        coalesce_mouse_moves(vec![(1, mv(1.0)), (2, mv(2.0))]),
        vec![(1, mv(1.0)), (2, mv(2.0))]
    );
}

#[test]
fn test_browser_input_event_for_key_maps_a_printable_character_to_type_text() {
    assert_eq!(
        browser_input_event_for_key(keyboard_types::Key::Character("a".to_string()), no_mods()),
        Some(BrowserInputEvent::TypeText {
            text: "a".to_string()
        })
    );
}

#[test]
fn test_browser_input_event_for_key_maps_each_recognized_named_key_to_press_key() {
    let cases = [
        (keyboard_types::Key::Enter, "Enter"),
        (keyboard_types::Key::Backspace, "Backspace"),
        (keyboard_types::Key::Tab, "Tab"),
        (keyboard_types::Key::Escape, "Escape"),
        (keyboard_types::Key::Delete, "Delete"),
        (keyboard_types::Key::ArrowUp, "ArrowUp"),
        (keyboard_types::Key::ArrowDown, "ArrowDown"),
        (keyboard_types::Key::ArrowLeft, "ArrowLeft"),
        (keyboard_types::Key::ArrowRight, "ArrowRight"),
    ];
    for (key, expected_name) in cases {
        assert_eq!(
            browser_input_event_for_key(key, no_mods()),
            Some(BrowserInputEvent::PressKey {
                key: expected_name.to_string(),
                modifiers: 0,
            }),
            "expected {expected_name} to forward as a PressKey"
        );
    }
}

#[test]
fn test_browser_input_event_for_key_drops_an_unrecognized_named_key() {
    // F1 isn't in the forwarded set — same reasoning
    // `browsing::server::named_key_event_fields` uses server-side: no
    // point round-tripping a key the server would reject anyway.
    assert_eq!(browser_input_event_for_key(keyboard_types::Key::F1, no_mods()), None);
}

#[test]
fn test_browser_input_event_for_key_drops_a_bare_modifier() {
    assert_eq!(browser_input_event_for_key(keyboard_types::Key::Shift, no_mods()), None);
    assert_eq!(browser_input_event_for_key(keyboard_types::Key::Control, no_mods()), None);
    assert_eq!(browser_input_event_for_key(keyboard_types::Key::Alt, no_mods()), None);
}
