//! A response body's bytes, handed out a whole line at a time (SME-169).

/// Bytes of an SSE body, handed out a whole line at a time, so a
/// character split across two network chunks is decoded whole (SME-169).
/// Before, each chunk was decoded on its own, and a character whose bytes
/// straddled two chunks became U+FFFD in the reply or a tool's input.
#[derive(Default)]
pub(super) struct LineBuffer {
    bytes: Vec<u8>,
    /// How far `bytes` is known to hold no `\n`, so a long line arriving in
    /// many chunks isn't searched from its start each time.
    scanned: usize,
}

impl LineBuffer {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    /// The next whole line, without its `\n` (a `\r` before it is left to
    /// the caller's `trim`), decoded lossily: bytes that aren't UTF-8
    /// *within* a line still become U+FFFD. `None` until a line's `\n` has
    /// arrived; a last line with none is never returned.
    pub(super) fn next_line(&mut self) -> Option<String> {
        let Some(offset) = self.bytes[self.scanned..].iter().position(|&b| b == b'\n') else {
            self.scanned = self.bytes.len();
            return None;
        };
        let end = self.scanned + offset;
        let line = String::from_utf8_lossy(&self.bytes[..end]).into_owned();
        self.bytes.drain(..=end);
        self.scanned = 0;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2-, 3- and 4-byte character in an event line, a JSON data line and
    /// a plain line, with a `\r\n` ending and the blank line ending an event.
    const BODY: &str = concat!(
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"é 中 🦀\"}}\n",
        "plain é中🦀\n",
        "\n",
    );

    fn expected() -> Vec<String> {
        let mut lines: Vec<String> = BODY.split('\n').map(str::to_string).collect();
        assert_eq!(lines.pop().as_deref(), Some(""), "the body ends with a newline");
        lines
    }

    fn feed(chunks: &[&[u8]]) -> Vec<String> {
        let mut buffer = LineBuffer::default();
        let mut lines = Vec::new();
        for chunk in chunks {
            buffer.push(chunk);
            while let Some(line) = buffer.next_line() {
                lines.push(line);
            }
        }
        lines
    }

    #[test]
    fn test_a_body_split_in_two_at_every_byte_gives_the_whole_lines() {
        let body = BODY.as_bytes();
        for cut in 0..=body.len() {
            let lines = feed(&[&body[..cut], &body[cut..]]);
            assert_eq!(lines, expected(), "split at byte {cut}");
            assert!(!lines.concat().contains('\u{FFFD}'), "split at byte {cut}");
        }
    }

    #[test]
    fn test_a_body_fed_one_byte_at_a_time_gives_the_whole_lines() {
        let chunks: Vec<&[u8]> = BODY.as_bytes().chunks(1).collect();
        assert_eq!(feed(&chunks), expected());
    }

    #[test]
    fn test_a_line_is_returned_only_once_its_newline_arrives() {
        let mut buffer = LineBuffer::default();
        buffer.push(b"data: ab");
        assert_eq!(buffer.next_line(), None);
        buffer.push(b"c");
        assert_eq!(buffer.next_line(), None);
        buffer.push(b"\r\nnext");
        assert_eq!(buffer.next_line().as_deref(), Some("data: abc\r"));
        assert_eq!(buffer.next_line(), None);
    }

    #[test]
    fn test_bytes_that_are_not_utf8_within_a_line_still_decode_lossily() {
        assert_eq!(feed(&[b"a\xffb\nc\n"]), vec!["a\u{FFFD}b".to_string(), "c".to_string()]);
    }
}
