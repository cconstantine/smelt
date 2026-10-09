//! Keeping a scrolled view stuck to its bottom (`use_sticky_bottom`), and
//! the transcript's text under the pointer still (SME-75).

use super::*;

/// How close to a scrollable container's bottom edge still counts as "at
/// the bottom" for auto-scroll purposes — a little slack for sub-pixel
/// layout rounding, not a meaningful reading gesture.
pub(super) const SCROLL_BOTTOM_SLACK_PX: f64 = 32.0;

/// Whether a scrollable container is close enough to its bottom edge that
/// new content should pull the view down with it. Both the message
/// transcript and each task's terminal body use this via their own
/// `onscroll` handler to decide, independently, whether the user has
/// scrolled up to read something (in which case new content must leave
/// their position alone) or is following along at the bottom (in which
/// case it should keep tracking new content, the way a real terminal
/// does).
pub(super) fn is_scrolled_to_bottom(scroll_top: f64, scroll_height: f64, client_height: f64) -> bool {
    scroll_height - scroll_top - client_height <= SCROLL_BOTTOM_SLACK_PX
}

/// A scrolled element that follows its content's bottom while the user is
/// at it, like `tail -f`, and leaves them be once they scroll up to read
/// (SME-83). The transcript and each sandbox terminal hold one
/// (`use_sticky_bottom`). The transcript also holds the text under the
/// pointer still through a layout change (SME-75); that part is its own,
/// below, since the terminals have no use for it.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct StickyBottom {
    el: Signal<Option<MountedEvent>>,
    stuck: Signal<bool>,
    /// The last scroll event's position and client height: whether the view
    /// can have moved on its own since then, and whether the viewport did. See
    /// `scrolled`.
    last_scroll: Signal<Option<(f64, f64)>>,
}

/// A `StickyBottom` that starts stuck, so new content shows.
pub(super) fn use_sticky_bottom() -> StickyBottom {
    StickyBottom {
        el: use_signal(|| None),
        stuck: use_signal(|| true),
        last_scroll: use_signal(|| None),
    }
}

impl StickyBottom {
    /// For the element's `onmounted`.
    pub(super) fn mounted(mut self, evt: MountedEvent) {
        self.el.set(Some(evt));
    }

    /// For the element's `onscroll`: whether the user is still at the
    /// bottom.
    ///
    /// A resize, a zoom or any other layout change fires the event with a
    /// viewport height the previous event never had, and the browser puts the
    /// view where its own clamp says it must go (SME-108: the clamp looked
    /// exactly like a scroll-up, so the terminal — and the transcript, same
    /// hook — stopped following its bottom for good after a single resize,
    /// with nothing the user had done to ask for that; and growing the window
    /// clamps a reader down *onto* the bottom, which read as a request to
    /// start following again a moment after they'd asked not to be). Such an
    /// event says nothing about intent, so the follow stays exactly as it
    /// was. With the viewport height steady, the position is the user's own
    /// and does say: at the bottom means following again, up from where they
    /// were means they stopped. Nothing else scrolls the view while the
    /// follow is off — the browser's own habit of pinning a scroller that sits
    /// at its end onto each new bottom is turned off for these elements by
    /// `overflow-anchor: none` (assets/chat.css), which is what makes that
    /// reading safe. The scroll height is not part of the test: it grows with
    /// streaming output on every event, whether or not the user is scrolling.
    pub(super) fn scrolled(mut self, data: &ScrollData) {
        let (top, height, client) =
            (data.scroll_top(), data.scroll_height() as f64, data.client_height() as f64);
        let last = *self.last_scroll.peek();
        self.last_scroll.set(Some((top, client)));
        let Some((last_top, last_client)) = last else {
            // The first event since the element mounted, so nothing can have
            // resized yet: the position is the view's own, and at the bottom
            // means following.
            if is_scrolled_to_bottom(top, height, client) {
                self.stuck.set(true);
            }
            return;
        };
        if (client - last_client).abs() >= 0.5 {
            return;
        }
        if is_scrolled_to_bottom(top, height, client) {
            self.stuck.set(true);
        } else if top < last_top - 1.0 {
            self.stuck.set(false);
        }
    }

    /// Back to following the bottom, as when a conversation opens.
    pub(super) fn stick(mut self) {
        self.stuck.set(true);
    }

    /// Whether the user is following the bottom. Read without subscribing:
    /// a scroll sets it on every event, and an effect that reran on each one
    /// pulled a small scroll up back down (SME-83).
    pub(super) fn is_stuck(self) -> bool {
        *self.stuck.peek()
    }

    /// The element once mounted. Subscribes, so an effect that waits for it
    /// reruns when it mounts.
    pub(super) fn el(self) -> Option<MountedEvent> {
        self.el.read().clone()
    }

    /// The element once mounted, without subscribing.
    pub(super) fn el_untracked(self) -> Option<MountedEvent> {
        self.el.peek().clone()
    }
}

/// Installed on the transcript once it mounts: remembers the element under
/// the pointer and where it was on screen, kept current as the pointer
/// moves and the transcript scrolls (SME-75).
pub(super) const TRANSCRIPT_ANCHOR_SETUP: &str = "const m = document.querySelector('.messages'); \
    if (m && !m.__smeltAnchor) { m.__smeltAnchor = true; \
      const remember = (x, y) => { const e = document.elementFromPoint(x, y); \
        window.__smeltTranscriptAnchor = e && e !== m && m.contains(e) \
          ? { el: e, top: e.getBoundingClientRect().top, x, y } : null; }; \
      m.addEventListener('pointermove', ev => remember(ev.clientX, ev.clientY)); \
      m.addEventListener('scroll', () => { const a = window.__smeltTranscriptAnchor; if (a) remember(a.x, a.y); }); \
      m.addEventListener('pointerleave', () => { window.__smeltTranscriptAnchor = null; }); }";

/// After a layout change with the pointer over the transcript: scrolls it
/// by however far the remembered element moved, so what's under the
/// pointer stays where it was, whichever way the layout moved it. Says
/// `none` when there was no such element, `bottom` when the transcript is
/// at its bottom afterwards (nothing left to snap), else `kept`.
pub(super) const TRANSCRIPT_KEEP_ANCHOR: &str = "const m = document.querySelector('.messages'); \
    const a = window.__smeltTranscriptAnchor; \
    if (!(m && a && a.el.isConnected)) { return 'none'; } \
    const d = a.el.getBoundingClientRect().top - a.top; \
    if (d) { m.scrollTop += d; } a.top = a.el.getBoundingClientRect().top; \
    return m.scrollHeight - m.scrollTop - m.clientHeight <= 1 ? 'bottom' : 'kept';";

/// A snap to the bottom for new content: the remembered element's position
/// is forgotten first, so a layout change handled in the same render
/// doesn't scroll back by the snap's own movement (SME-75 code review 2).
pub(super) const TRANSCRIPT_FORGET_ANCHOR: &str = "window.__smeltTranscriptAnchor = null;";

/// After a layout change with the pointer over the transcript: keeps what's
/// under the pointer in place (see `TRANSCRIPT_KEEP_ANCHOR`), or, when
/// there was nothing under it (the messages had only just arrived), snaps
/// to the bottom as if the pointer weren't there. Either way the pending
/// snap is settled here or on the pointer leaving.
pub(super) async fn keep_transcript_anchor(el: MountedEvent, mut pending: impl Writable<Target = bool> + 'static) {
    let outcome = document::eval(TRANSCRIPT_KEEP_ANCHOR).await.ok();
    match outcome.as_ref().and_then(|v| v.as_str()) {
        Some("kept") => {}
        Some("bottom") => pending.set(false),
        _ => {
            pending.set(false);
            scroll_to_bottom(el).await;
        }
    }
}

/// The content effect's snap: forgets the anchor, then scrolls to the
/// bottom (see `TRANSCRIPT_FORGET_ANCHOR`).
pub(super) async fn snap_transcript_for_content(el: MountedEvent) {
    let _ = document::eval(TRANSCRIPT_FORGET_ANCHOR).await;
    scroll_to_bottom(el).await;
}

/// Scrolls `el` to its bottom at once.
pub(super) async fn scroll_to_bottom(el: MountedEvent) {
    if let Ok(size) = el.get_scroll_size().await {
        let _ = el
            .scroll(PixelsVector2D::new(0.0, size.height), ScrollBehavior::Instant)
            .await;
    }
}
