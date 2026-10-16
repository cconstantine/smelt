use super::*;

#[test]
fn test_parse_elements_parses_a_well_formed_list() {
    let json = r#"[
                {"index": 0, "tag": "a", "kind": "link", "label": "Home"},
                {"index": 1, "tag": "input", "kind": "input", "label": "Search"}
            ]"#;
    let elements = parse_elements(json).expect("should parse");
    assert_eq!(
        elements,
        vec![
            PageElement {
                index: 0,
                tag: "a".to_string(),
                kind: "link".to_string(),
                label: "Home".to_string(),
            },
            PageElement {
                index: 1,
                tag: "input".to_string(),
                kind: "input".to_string(),
                label: "Search".to_string(),
            },
        ]
    );
}

#[test]
fn test_parse_elements_parses_an_empty_list() {
    assert_eq!(parse_elements("[]").expect("should parse"), vec![]);
}

#[test]
fn test_parse_elements_rejects_malformed_json() {
    assert!(parse_elements("not json").is_err());
}

#[test]
fn test_parse_elements_rejects_an_element_missing_a_field() {
    assert!(parse_elements(r#"[{"index": 0, "tag": "a"}]"#).is_err());
}

#[test]
fn test_named_key_event_fields_accepts_a_known_key() {
    assert_eq!(
        named_key_event_fields("Enter").expect("Enter should be known"),
        NamedKey {
            key: "Enter",
            code: "Enter",
            windows_virtual_key_code: 13,
            text: Some("\r"),
        }
    );
}

#[test]
fn test_named_key_event_fields_rejects_an_unknown_key() {
    assert!(named_key_event_fields("F13").is_err());
}

#[test]
fn test_key_event_params_sends_enter_down_with_its_text() {
    let params = key_event_params("Enter", 0).expect("Enter should be known");
    assert_eq!(params.len(), 2);
    assert_eq!(params[0].r#type, DispatchKeyEventType::KeyDown);
    assert_eq!(params[0].text.as_deref(), Some("\r"));
    assert_eq!(params[1].r#type, DispatchKeyEventType::KeyUp);
    assert_eq!(params[1].text, None);
}

#[test]
fn test_key_event_params_sends_a_non_typing_key_down_raw() {
    let params = key_event_params("Backspace", 0).expect("Backspace should be known");
    assert_eq!(params[0].r#type, DispatchKeyEventType::RawKeyDown);
    assert_eq!(params[0].text, None);
    assert_eq!(params[1].r#type, DispatchKeyEventType::KeyUp);
}

fn buttons_of(event: BrowserInputEvent) -> (Option<MouseButton>, Option<i64>) {
    let params = mouse_event_params(&event);
    (params.button, params.buttons)
}

use chromiumoxide::cdp::browser_protocol::input::MouseButton;

#[test]
fn test_mouse_event_params_hover_holds_no_button() {
    assert_eq!(
        buttons_of(BrowserInputEvent::MouseMove { x: 1.0, y: 2.0, left_held: false }),
        (Some(MouseButton::None), Some(0))
    );
}

#[test]
fn test_mouse_event_params_drag_holds_the_left_button() {
    assert_eq!(
        buttons_of(BrowserInputEvent::MouseMove { x: 1.0, y: 2.0, left_held: true }),
        (Some(MouseButton::Left), Some(1))
    );
}

#[test]
fn test_mouse_event_params_press_and_release_name_the_left_button() {
    assert_eq!(
        buttons_of(BrowserInputEvent::MouseDown { x: 1.0, y: 2.0 }),
        (Some(MouseButton::Left), Some(1))
    );
    assert_eq!(
        buttons_of(BrowserInputEvent::MouseUp { x: 1.0, y: 2.0 }),
        (Some(MouseButton::Left), Some(0))
    );
}

#[test]
fn test_mouse_event_params_wheel_holds_no_button() {
    let params = mouse_event_params(&BrowserInputEvent::Wheel {
        x: 1.0,
        y: 2.0,
        delta_x: 0.0,
        delta_y: 120.0,
    });
    assert_eq!((params.button, params.buttons), (Some(MouseButton::None), Some(0)));
    assert_eq!(params.delta_y, Some(120.0));
}

/// Each `(typed, expected)` case that `normalize_address` gets wrong,
/// so a failure names every wrong case at once rather than the first.
fn wrong_addresses(cases: &[(&str, &str)]) -> Vec<String> {
    cases
        .iter()
        .filter_map(|&(typed, expected)| {
            let got = normalize_address(typed);
            (got.as_deref() != Ok(expected))
                .then(|| format!("{typed:?}: expected {expected:?}, got {got:?}"))
        })
        .collect()
}

#[test]
fn test_normalize_address_gives_local_and_private_addresses_http() {
    // SME-184: as in Chromium's omnibox, an IP literal, an explicit port
    // other than 443, a single-label name or a local-only suffix gets
    // plain http, since a dev server in the sandbox speaks no TLS.
    let wrong = wrong_addresses(&[
        ("localhost:5173", "http://localhost:5173"),
        ("localhost", "http://localhost"),
        ("LOCALHOST:5173/x", "http://LOCALHOST:5173/x"),
        ("localhost:8080/a?b=1", "http://localhost:8080/a?b=1"),
        ("app.localhost:5173", "http://app.localhost:5173"),
        ("app.localhost", "http://app.localhost"),
        ("127.0.0.1:3000", "http://127.0.0.1:3000"),
        ("127.1", "http://127.1"),
        ("10.0.0.5", "http://10.0.0.5"),
        ("192.168.1.20:8080", "http://192.168.1.20:8080"),
        ("172.16.0.1", "http://172.16.0.1"),
        ("[::1]:5173", "http://[::1]:5173"),
        ("[fd00::1]", "http://[fd00::1]"),
        ("8.8.8.8", "http://8.8.8.8"),
        ("myhost", "http://myhost"),
        ("myhost/path", "http://myhost/path"),
        ("nas.lan", "http://nas.lan"),
        ("foo.internal", "http://foo.internal"),
        ("printer.local", "http://printer.local"),
        ("site.test", "http://site.test"),
        ("x.home.arpa", "http://x.home.arpa"),
        ("localhost.", "http://localhost."),
        ("example.com:8443", "http://example.com:8443"),
        // Review 1: a typed :80 is http's own port; https on it can't load.
        ("example.com:80", "http://example.com:80"),
        ("nas.corp:80", "http://nas.corp:80"),
        // Review 1: a `://` later on (a return URL in the query) isn't a scheme.
        ("localhost:5173/?next=http://x", "http://localhost:5173/?next=http://x"),
    ]);
    assert!(wrong.is_empty(), "wrong scheme for:\n{}", wrong.join("\n"));
}

#[test]
fn test_normalize_address_gives_public_names_https() {
    let wrong = wrong_addresses(&[
        ("example.com", "https://example.com"),
        ("example.com/a?b=1", "https://example.com/a?b=1"),
        ("sub.example.co.uk", "https://sub.example.co.uk"),
        ("example.com/r?u=https://y", "https://example.com/r?u=https://y"),
        // Only a whole label matches a local suffix: `contest` isn't `test`.
        ("shop.contest", "https://shop.contest"),
        // A private suffix not on the short list stays https (a trade-off).
        ("nas.corp", "https://nas.corp"),
        // 443 is https's own port (Chromium would send http, which fails).
        ("example.com:443", "https://example.com:443"),
        // Not a parseable host: kept as before, for `navigate` to refuse.
        ("::1", "https://::1"),
        ("localhost:99999", "https://localhost:99999"),
    ]);
    assert!(wrong.is_empty(), "wrong scheme for:\n{}", wrong.join("\n"));
}

#[test]
fn test_normalize_address_keeps_an_explicit_scheme_and_trims() {
    assert_eq!(normalize_address("  http://example.com/x  ").unwrap(), "http://example.com/x");
    assert_eq!(normalize_address("https://example.com").unwrap(), "https://example.com");
    assert_eq!(normalize_address("HTTP://Example.com").unwrap(), "HTTP://Example.com");
    assert_eq!(normalize_address("chrome://version").unwrap(), "chrome://version");
    assert_eq!(
        normalize_address("http://localhost:5173/?next=http://x").unwrap(),
        "http://localhost:5173/?next=http://x"
    );
}

#[test]
fn test_normalize_address_rejects_nothing() {
    assert!(normalize_address("").is_err());
    assert!(normalize_address("   ").is_err());
}

#[test]
fn test_key_event_params_carries_modifiers_on_both_events() {
    let params = key_event_params("Tab", 8).expect("Tab should be known");
    assert!(params.iter().all(|p| p.modifiers == Some(8)));
}

fn frame(data: &str) -> Option<BrowserFrame> {
    Some(BrowserFrame {
        data: data.to_string(),
    })
}

/// A subscription not counted against any real session (no
/// session has this id), so dropping it touches nothing.
fn detached_subscription(
    rx: watch::Receiver<Option<BrowserFrame>>,
) -> FrameSubscription {
    FrameSubscription::new(
        rx,
        ViewerGuard {
            conversation_id: -1,
            session_id: 0,
        },
    )
}

#[tokio::test]
async fn test_a_new_viewer_gets_the_latest_frame_straight_away() {
    let (tx, rx) = watch::channel(None);
    tx.send_replace(frame("a"));
    tx.send_replace(frame("b"));
    let mut sub = detached_subscription(rx);
    let first = tokio::time::timeout(Duration::from_millis(100), sub.next_frame()).await;
    assert_eq!(first.expect("should not wait"), frame("b"));
}

#[tokio::test]
async fn test_a_slow_viewer_skips_to_the_newest_frame() {
    let (tx, rx) = watch::channel(frame("a"));
    let mut sub = detached_subscription(rx);
    assert_eq!(sub.next_frame().await, frame("a"));
    tx.send_replace(frame("b"));
    tx.send_replace(frame("c"));
    assert_eq!(sub.next_frame().await, frame("c"));
}

#[tokio::test]
async fn test_a_viewer_waits_while_there_is_no_frame_yet() {
    let (tx, rx) = watch::channel(None);
    let mut sub = detached_subscription(rx);
    let early = tokio::time::timeout(Duration::from_millis(50), sub.next_frame()).await;
    assert!(early.is_err(), "should still be waiting, got {early:?}");
    tx.send_replace(frame("a"));
    assert_eq!(sub.next_frame().await, frame("a"));
}

#[tokio::test]
async fn test_the_frame_stream_ends_when_the_session_closes() {
    let (tx, rx) = watch::channel(frame("a"));
    let mut stream = Box::pin(frame_stream(detached_subscription(rx)));
    assert!(matches!(stream.next().await, Some(Ok(_))));
    drop(tx);
    assert!(stream.next().await.is_none());
}

/// A call that panics while holding the session list must not
/// break every later browsing call (SME-119). The poisoning is
/// process-wide on purpose: the frame tests above then also run
/// on a recovered lock. One of them, running in parallel, may be
/// the call that recovers it, so the test doesn't assert the lock
/// is still poisoned once the thread has panicked (std poisons it
/// whenever a guard drops during a panic).
#[tokio::test]
async fn test_a_poisoned_session_lock_does_not_break_the_next_browsing_call() {
    let poisoner = std::thread::spawn(|| {
        let _held = SESSIONS.lock();
        panic!("a browsing call panics while holding the session list (deliberate, SME-119)");
    });
    assert!(poisoner.join().is_err(), "the poisoning thread should have panicked");

    let unused: i64 = -119;
    assert_eq!(current_url(unused), None);
    let subscribed = subscribe_frames(unused).map(|_| ());
    assert_eq!(
        subscribed,
        Err("no browser session is open for this conversation".to_string())
    );
    let navigated = navigate(unused, "https://example.com/").await.map(|_| ());
    assert!(
        navigated
            .as_ref()
            .is_err_and(|e| e.starts_with("no browser session is open")),
        "expected the ordinary no-session error, got {navigated:?}"
    );
    record_url(unused, 0, "https://example.com/".to_string());
    let (_tx, rx) = watch::channel(None);
    drop(detached_subscription(rx));
    assert_eq!(close_session(unused).await, Ok(()));
    assert!(!SESSIONS.is_poisoned(), "the lock should be healthy again");
}
