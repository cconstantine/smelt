//! Copying text to the clipboard from a click, over HTTPS and plain HTTP
//! alike (SME-105).
//!
//! `navigator.clipboard` exists only in a secure context, so on a LAN
//! address such as `http://ryzen.lan:8180` it is `undefined`. There the copy
//! falls back to selecting a hidden `textarea` and `document.execCommand('copy')`,
//! which is deprecated but still supported for `copy` and not limited to
//! secure contexts (open-webui's `copyToClipboard` does the same).
//!
//! Both paths need the user's click: `copy_text` starts its script while the
//! click handler is still running. dioxus-web runs a handler synchronously
//! inside the DOM event, and `document::eval` runs the script up to its
//! first `await` as soon as it's called (dioxus-web 0.7.9,
//! `WebEvaluator::create`), so only the wait for the result is spawned.

use dioxus::prelude::*;

/// How long "Copied" or "Couldn't copy" shows before the label goes back.
#[cfg(feature = "web")]
const FEEDBACK_MS: u32 = 1500;

/// Waits while a click's feedback shows. Only a click in the browser
/// reaches it; the server never runs a click handler.
async fn feedback_shown() {
    #[cfg(feature = "web")]
    gloo_timers::future::TimeoutFuture::new(FEEDBACK_MS).await;
}

/// The script for `copy_text`: `writeText` when the Clipboard API is there,
/// else the `textarea` fallback. Returns whether the text was copied.
fn copy_script(text: &str) -> String {
    // A JSON string is a valid JavaScript string literal.
    let literal = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"const text = {literal};
        const writeText = navigator.clipboard?.writeText?.bind(navigator.clipboard);
        if (writeText) {{
            try {{ await writeText(text); return true; }} catch (_) {{}}
        }}
        const area = document.createElement('textarea');
        area.value = text;
        area.setAttribute('readonly', '');
        area.setAttribute('aria-hidden', 'true');
        area.style.cssText = 'position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;pointer-events:none';
        const before = document.activeElement;
        document.body.appendChild(area);
        let copied = false;
        try {{
            area.focus({{ preventScroll: true }});
            area.select();
            area.setSelectionRange(0, text.length);
            copied = document.execCommand('copy') === true;
        }} catch (_) {{
            copied = false;
        }} finally {{
            area.remove();
            if (before && typeof before.focus === 'function') before.focus({{ preventScroll: true }});
        }}
        return copied;"#
    )
}

/// Copies `text`, and resolves to whether it was copied. The script starts
/// before this returns, so call it in the click handler itself, not inside
/// `spawn`: that keeps the copy inside the user's gesture.
pub(crate) fn copy_text(text: &str) -> impl std::future::Future<Output = bool> + use<> {
    let eval = document::eval(&copy_script(text));
    async move { matches!(eval.await, Ok(serde_json::Value::Bool(true))) }
}

/// A copy button's label state: which click is current, and its outcome
/// once known. A click's result or timer only counts while it's the newest
/// click, so a second click's label isn't cleared early by the first one's
/// timer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct CopyFeedback {
    generation: u32,
    /// `Some(true)` "Copied", `Some(false)` "Couldn't copy", `None` the
    /// button's own label (also while a copy is still in progress).
    outcome: Option<bool>,
}

impl CopyFeedback {
    /// A click: a new generation, whose outcome isn't known yet.
    pub(crate) fn clicked(&mut self) -> u32 {
        self.generation = self.generation.wrapping_add(1);
        self.outcome = None;
        self.generation
    }

    /// Click `generation`'s copy finished.
    pub(crate) fn finished(&mut self, generation: u32, copied: bool) {
        if generation == self.generation {
            self.outcome = Some(copied);
        }
    }

    /// Click `generation`'s feedback has shown long enough.
    pub(crate) fn expired(&mut self, generation: u32) {
        if generation == self.generation {
            self.outcome = None;
        }
    }

    pub(crate) fn outcome(&self) -> Option<bool> {
        self.outcome
    }
}

/// A button that copies `text`. Its label shows "Copied" or "Couldn't copy"
/// for a moment after a click. `on_highlight` hears `true` while the button
/// is pointed at, focused from the keyboard, or showing its feedback, and
/// `false` once none of those holds: the reply's button outlines what it
/// copies (SME-105).
#[component]
pub(crate) fn CopyButton(
    text: std::rc::Rc<str>,
    label: String,
    class: &'static str,
    title: String,
    on_highlight: Option<EventHandler<bool>>,
) -> Element {
    let mut feedback = use_signal(CopyFeedback::default);
    let mut hovered = use_signal(|| false);
    let mut keyboard_focus = use_signal(|| false);
    // A press focuses the button too (in Chrome; not in Safari or Firefox
    // on macOS); that focus isn't the keyboard's. Cleared when the press
    // ends, so a later Tab onto the button counts (code review 1).
    let mut pressed = use_signal(|| false);
    let mut highlighted = use_signal(|| false);
    // Set while the copy script's synchronous part runs: the fallback
    // focuses its textarea and then this button again, inside the click,
    // and dioxus-web runs those focus and blur handlers there and then.
    // They aren't the user's (code review 1).
    // Read with `peek`, so nothing re-renders for it.
    let mut copying = use_signal(|| false);
    let mut report = move || {
        let on = *hovered.peek() || *keyboard_focus.peek() || feedback.peek().outcome().is_some();
        if on != *highlighted.peek() {
            highlighted.set(on);
            if let Some(handler) = on_highlight {
                handler.call(on);
            }
        }
    };
    let copy = move |_| {
        let generation = feedback.write().clicked();
        copying.set(true);
        let copied = copy_text(&text);
        copying.set(false);
        spawn(async move {
            let copied = copied.await;
            feedback.write().finished(generation, copied);
            report();
            feedback_shown().await;
            feedback.write().expired(generation);
            report();
        });
    };
    let shown = match feedback().outcome() {
        Some(true) => "Copied".to_string(),
        Some(false) => "Couldn't copy".to_string(),
        None => label,
    };
    rsx! {
        button {
            class,
            r#type: "button",
            title,
            onclick: copy,
            onpointerenter: move |_| {
                hovered.set(true);
                report();
            },
            onpointerleave: move |_| {
                hovered.set(false);
                report();
            },
            onpointerdown: move |_| pressed.set(true),
            onpointerup: move |_| pressed.set(false),
            onpointercancel: move |_| pressed.set(false),
            onfocus: move |_| {
                if !*pressed.peek() && !*copying.peek() {
                    keyboard_focus.set(true);
                    report();
                }
            },
            onblur: move |_| {
                if *copying.peek() {
                    return;
                }
                pressed.set(false);
                keyboard_focus.set(false);
                report();
            },
            "{shown}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_click_shows_its_outcome_until_its_timer_expires() {
        let mut feedback = CopyFeedback::default();
        let click = feedback.clicked();
        assert_eq!(feedback.outcome(), None, "no outcome while the copy runs");
        feedback.finished(click, true);
        assert_eq!(feedback.outcome(), Some(true));
        feedback.expired(click);
        assert_eq!(feedback.outcome(), None);
    }

    #[test]
    fn test_a_failed_copy_says_so() {
        let mut feedback = CopyFeedback::default();
        let click = feedback.clicked();
        feedback.finished(click, false);
        assert_eq!(feedback.outcome(), Some(false));
    }

    #[test]
    fn test_an_older_clicks_timer_does_not_clear_a_newer_clicks_label() {
        let mut feedback = CopyFeedback::default();
        let first = feedback.clicked();
        feedback.finished(first, true);
        let second = feedback.clicked();
        assert_ne!(first, second, "each click is its own generation");
        feedback.finished(second, false);
        feedback.expired(first);
        assert_eq!(feedback.outcome(), Some(false), "the second click's label stays");
        feedback.expired(second);
        assert_eq!(feedback.outcome(), None);
    }

    #[test]
    fn test_an_older_clicks_late_result_does_not_overwrite_a_newer_one() {
        let mut feedback = CopyFeedback::default();
        let first = feedback.clicked();
        let second = feedback.clicked();
        feedback.finished(second, true);
        feedback.finished(first, false);
        assert_eq!(feedback.outcome(), Some(true));
    }

    #[test]
    fn test_the_copy_script_carries_the_text_as_one_string_literal() {
        let text = "a `b` ${c}\n</script>\"d\"\\";
        let script = copy_script(text);
        let literal = serde_json::to_string(text).expect("a string serializes");
        assert!(script.starts_with(&format!("const text = {literal};")), "{script}");
        assert!(script.contains("execCommand('copy')"), "the fallback: {script}");
        assert!(script.contains("navigator.clipboard?.writeText"), "the secure path: {script}");
    }
}
