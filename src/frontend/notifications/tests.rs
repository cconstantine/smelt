use super::*;
use crate::events::Attention;

/// The plan's mode table, every row: what the browser offers (secure
/// context, `Notification`, `navigator.locks`), what it allowed
/// (permission, constructor throws), and what the user set.
#[test]
fn test_notify_mode_covers_the_whole_table() {
    let mode = |secure, notification, locks, permission, throws, setting| {
        notify_mode(secure, notification, locks, permission, throws, setting)
    };
    // Unavailable outranks everything: plain HTTP with a granted
    // permission from an earlier secure visit still can't notify.
    assert_eq!(mode(false, true, true, Permission::Granted, false, true), NotifyMode::Unavailable);
    assert_eq!(mode(true, false, true, Permission::Granted, false, true), NotifyMode::Unavailable);
    assert_eq!(mode(true, true, false, Permission::Granted, false, true), NotifyMode::Unavailable);
    // Unsupported: the constructor threw (Chrome on Android) — learned
    // by trying, never guessed — outranks Blocked and the setting.
    assert_eq!(mode(true, true, true, Permission::Granted, true, true), NotifyMode::Unsupported);
    assert_eq!(mode(true, true, true, Permission::Denied, true, false), NotifyMode::Unsupported);
    // Blocked: denied permission, whatever the setting says.
    assert_eq!(mode(true, true, true, Permission::Denied, false, true), NotifyMode::Blocked);
    // On needs both the setting and a granted permission; a granted
    // permission alone is Off (the setting is per browser, remember).
    assert_eq!(mode(true, true, true, Permission::Granted, false, true), NotifyMode::On);
    assert_eq!(mode(true, true, true, Permission::Granted, false, false), NotifyMode::Off);
    // Never asked (or the prompt dismissed) is Off: the button asks.
    assert_eq!(mode(true, true, true, Permission::NotAsked, false, true), NotifyMode::Off);
    assert_eq!(mode(true, true, true, Permission::NotAsked, false, false), NotifyMode::Off);
}

/// Only Off and On answer a click, and only Unavailable explains itself.
#[test]
fn test_modes_open_up_or_stay_shut_by_label_and_click() {
    assert_eq!(
        [NotifyMode::Unavailable, NotifyMode::Unsupported, NotifyMode::Blocked, NotifyMode::Off, NotifyMode::On]
            .map(|m| (m.label(), m.enabled())),
        [
            ("Desktop notifications need HTTPS", false),
            ("Notifications aren't supported in this browser", false),
            ("Notifications blocked in site settings", false),
            ("Turn on notifications", true),
            ("Notifications on", true),
        ]
    );
    assert!(NotifyMode::Unavailable.hint().is_some_and(|h| h.contains("https://localhost:8443")));
    assert_eq!(NotifyMode::Off.hint(), None);
    assert_eq!(NotifyMode::On.hint(), None);
}

/// The leader's steps before showing: the setting, and watching this
/// conversation in this visible, focused tab. No kind is held back by a
/// cooldown any more (SME-377): the automatic Finished it was for is gone.
#[test]
fn test_should_notify_walks_the_leaders_steps() {
    let ask = |setting, visible, focused, showing_this, _kind: Attention| should_notify(setting, visible, focused, showing_this);
    // The setting first: a tab whose setting is off (another tab may
    // have turned it off) notifies nothing, in any position.
    assert!(!ask(false, false, false, false, Attention::Question));
    // The user is looking at this very conversation: nothing.
    assert!(!ask(true, true, true, true, Attention::Question));
    // Focused and visible but on another page: the user is in smelt,
    // but not here — still a notification (the plan counts this one).
    assert!(ask(true, true, true, false, Attention::Question));
    // Hidden, or unfocused: notify.
    assert!(ask(true, false, true, true, Attention::Question));
    assert!(ask(true, true, false, true, Attention::Question));
    for kind in [Attention::Notified, Attention::Question, Attention::Failed, Attention::CutOff, Attention::Refused] {
        assert!(ask(true, false, false, false, kind), "{kind:?} is never held back");
    }
}

/// Heading per kind, and the body: the title, plus the first question's
/// header where there is one; for `notify_user` (SME-377), the title in
/// the heading and the model's message as the body.
#[test]
fn test_notification_text_names_what_happened_and_where() {
    assert_eq!(
        notification_text(Attention::Question, "fix the build", Some("Which one?"), None),
        ("smelt has a question".into(), "fix the build — Which one?".into())
    );
    // A question with no header (or an empty one) reads like the others.
    assert_eq!(
        notification_text(Attention::Question, "fix the build", None, None),
        ("smelt has a question".into(), "fix the build".into())
    );
    assert_eq!(
        notification_text(Attention::Question, "fix the build", Some(""), None),
        ("smelt has a question".into(), "fix the build".into())
    );
    for (kind, heading) in [
        (Attention::Failed, "smelt: the turn failed"),
        (Attention::CutOff, "smelt: the reply was cut off"),
        (Attention::Refused, "smelt: the model refused"),
    ] {
        assert_eq!(notification_text(kind, "title", None, None), (heading.into(), "title".into()));
        // A header or a message of another kind's does not sneak in.
        assert_eq!(notification_text(kind, "title", Some("h"), Some("m")).1, "title");
    }
    assert_eq!(
        notification_text(Attention::Notified, "fix the build", None, Some("Built and tested")),
        ("smelt: fix the build".into(), "Built and tested".into())
    );
    // No message (an older server's answer): the title is the body.
    assert_eq!(
        notification_text(Attention::Notified, "fix the build", None, None),
        ("smelt: fix the build".into(), "fix the build".into())
    );
}

/// A second event for one conversation replaces its notification:
/// the tag is the conversation, not the event.
#[test]
fn test_the_tag_is_the_conversation() {
    assert_eq!(notification_tag(42), "smelt-conversation-42");
    assert_eq!(notification_tag(42), notification_tag(42));
}

/// The title count, the notification that works where notifications can't.
#[test]
fn test_badge_title_carries_the_count() {
    assert_eq!(badge_title(0), "smelt");
    assert_eq!(badge_title(1), "(1) smelt");
    assert_eq!(badge_title(12), "(12) smelt");
}
