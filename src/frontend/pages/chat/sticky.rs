//! Keeping a scrolled view stuck to its bottom, and the transcript's text under the pointer still (SME-75).

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
pub(super) async fn keep_transcript_anchor(el: MountedEvent, mut pending: Signal<bool>) {
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
