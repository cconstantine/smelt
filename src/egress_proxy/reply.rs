//! Reading the pod's reply heads on the sandbox route the way Chrome reads
//! them, so the final one can be marked `Connection: close` (SME-165).
//!
//! The proxy checks a request once, when its connection opens. Chrome
//! pools a proxy connection per destination and reuses it for the next
//! request there whenever the final reply says keep-alive
//! (`HttpStreamParser::CanReuseConnection`, `HttpResponseHeaders::IsKeepAlive`),
//! and the proxy's own `Connection: close` on the request binds only a
//! server that honours it. A server in the pod that doesn't would let the
//! next request to that address reach it unchecked. So the final reply
//! head always says `close`: each connection carries one request.
//!
//! Where a head starts and ends, its status code and its header lines are
//! read as Chromium's `net/http` does (`HttpUtil::LocateStartOfStatusLine`,
//! `LocateEndOfHeaders`, `AssembleRawHeaders`; `HttpResponseHeaders::
//! ParseStatusLine`), cited from `chromium/src` `main` on 2026-10-16: a
//! head read differently here could let a keep-alive head through as an
//! interim one, or put the proxy's `close` on a head Chrome skips.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bytes allowed before a status line: `LocateStartOfStatusLine`'s slop.
const STATUS_LINE_SLOP: usize = 4;

/// Where the status line starts in `buf`: `http`, in any case, at most
/// `STATUS_LINE_SLOP` bytes in. `None` while `buf` is too short to say, or
/// when it isn't there.
fn status_line_start(buf: &[u8]) -> Option<usize> {
    let last = buf.len().checked_sub(4)?.min(STATUS_LINE_SLOP);
    (0..=last).find(|&i| buf[i..i + 4].eq_ignore_ascii_case(b"http"))
}

/// Whether `buf` can't be an HTTP reply: Chrome decides that once 8 bytes
/// have come without `http` in place, and reads the reply as HTTP/0.9,
/// which it never reuses (and refuses on a port other than 80).
fn is_not_http(buf: &[u8]) -> bool {
    buf.len() >= STATUS_LINE_SLOP + 4 && status_line_start(buf).is_none()
}

/// Just past the blank line ending the head that starts at `from`: a line
/// feed, at most one carriage return, and another line feed.
fn end_of_head(buf: &[u8], from: usize) -> Option<usize> {
    let mut last = 0u8;
    let mut was_lf = false;
    for (i, &c) in buf.iter().enumerate().skip(from) {
        if c == b'\n' {
            if was_lf {
                return Some(i + 1);
            }
            was_lf = true;
        } else if c != b'\r' || last != b'\n' {
            was_lf = false;
        }
        last = c;
    }
    None
}

/// `head`'s lines as Chrome splits them: NUL bytes dropped, then broken at
/// every run of carriage returns and line feeds. The first is the status
/// line.
fn head_lines(head: &[u8]) -> Vec<Vec<u8>> {
    let head: Vec<u8> = head.iter().copied().filter(|&b| b != 0).collect();
    head.split(|&b| b == b'\r' || b == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// The status code Chrome reads from `head` (which starts at its status
/// line): the digits after the line's first space and any more spaces
/// (not tabs), saturating; 200 when there are none.
pub(super) fn status_code(head: &[u8]) -> u64 {
    let lines = head_lines(head);
    let Some(line) = lines.first() else { return 200 };
    let Some(space) = line.iter().position(|&b| b == b' ') else { return 200 };
    let digits = line[space..].iter().skip_while(|&&b| b == b' ').take_while(|b| b.is_ascii_digit());
    let mut code: Option<u64> = None;
    for digit in digits {
        let value = code.unwrap_or(0);
        code = Some(value.saturating_mul(10).saturating_add(u64::from(digit - b'0')));
    }
    code.unwrap_or(200)
}

/// The hop-by-hop headers that say whether a connection is kept: the ones
/// Chrome's `IsKeepAlive` reads, and `Keep-Alive`'s parameters.
const CONNECTION_HEADERS: [&[u8]; 3] = [b"connection", b"proxy-connection", b"keep-alive"];

/// `head` (from its status line to its blank line) with `Connection:
/// close` as its first header and every other `Connection`,
/// `Proxy-Connection` and `Keep-Alive` header gone, a dropped header's
/// continuation lines with it, and any continuation line straight after the
/// status line. Lines are ended with CRLF; the status line
/// and every other header are kept as they were, so the body's framing is
/// unchanged. Being first, the `close` is the token `IsKeepAlive` meets
/// first even if a name here were read differently.
pub(super) fn close_response_head(head: &[u8]) -> Vec<u8> {
    let lines = head_lines(head);
    let mut out = Vec::with_capacity(head.len() + 19);
    let mut lines = lines.into_iter();
    if let Some(status) = lines.next() {
        out.extend_from_slice(&status);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n");
    // Whether the last line that wasn't a continuation was dropped. A
    // continuation line straight after the status line is dropped too:
    // Chrome would join it onto `Connection: close`, making that one value
    // neither `close` nor `keep-alive` (SME-165 review 1), and where it
    // follows the status line Chrome skips it anyway.
    let mut dropping = true;
    for line in lines {
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if !continuation {
            let name = line.split(|&b| b == b':').next().unwrap_or_default();
            dropping = line.contains(&b':')
                && CONNECTION_HEADERS.iter().any(|h| name.trim_ascii().eq_ignore_ascii_case(h));
        }
        if !dropping {
            out.extend_from_slice(&line);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// How relaying the pod's reply heads ended.
pub(super) enum Relayed {
    /// The final head went to the page (closed), or a `101` to an upgrade,
    /// or bytes that aren't HTTP: copy the rest raw.
    CopyRest,
    /// The pod's stream ended, with whatever came of the reply passed on.
    Ended,
    /// The stream ended before a single byte: nothing listens there.
    Nothing,
    /// A head grew past `limit` without ending: too large to check.
    TooLarge,
}

/// Reads the pod's reply heads and passes them to the page: interim (1xx)
/// heads unchanged, a `101` unchanged and the end of the heads when the
/// request was `upgrading`, and the final head through
/// `close_response_head`. The bytes after the last head handled go on to
/// the page too. No time limit: a slow server is legitimate.
pub(super) async fn relay_reply_heads(
    from_pod: &mut (impl AsyncRead + Unpin),
    to_page: &mut (impl AsyncWrite + Unpin),
    upgrading: bool,
    limit: usize,
) -> std::io::Result<Relayed> {
    let mut buf: Vec<u8> = Vec::new();
    let mut sent_any = false;
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        // Once a head has gone, Chrome reads more bytes without a status
        // line as an invalid reply (`has_seen_status_line_`); it reuses
        // nothing either way.
        if is_not_http(&buf) {
            to_page.write_all(&buf).await?;
            return Ok(if sent_any { Relayed::Ended } else { Relayed::CopyRest });
        }
        if let Some(start) = status_line_start(&buf)
            && let Some(end) = end_of_head(&buf, start)
        {
            if end - start > limit {
                return Ok(Relayed::TooLarge);
            }
            let after = buf.split_off(end);
            let head = &buf[start..];
            let code = status_code(head);
            sent_any = true;
            if code / 100 == 1 && !(code == 101 && upgrading) {
                to_page.write_all(head).await?;
                buf = after;
                continue;
            }
            if code == 101 {
                to_page.write_all(head).await?;
            } else {
                to_page.write_all(&close_response_head(head)).await?;
            }
            to_page.write_all(&after).await?;
            return Ok(Relayed::CopyRest);
        }
        if buf.len() > limit {
            return Ok(Relayed::TooLarge);
        }
        let n = from_pod.read(&mut chunk).await?;
        if n == 0 {
            if buf.is_empty() && !sent_any {
                return Ok(Relayed::Nothing);
            }
            to_page.write_all(&buf).await?;
            return Ok(Relayed::Ended);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(head: &str) -> String {
        String::from_utf8(close_response_head(head.as_bytes())).expect("UTF-8")
    }

    #[test]
    fn test_close_response_head_replaces_every_connection_header() {
        assert_eq!(
            closed(
                "HTTP/1.1 200 OK\r\nCONNECTION: keep-alive, Upgrade\r\ncontent-type: text/html\r\n\
                 KEEP-ALIVE: timeout=5\r\nproxy-connection: keep-alive\r\nX-Kept: 1\r\n\r\n"
            ),
            "HTTP/1.1 200 OK\r\nConnection: close\r\ncontent-type: text/html\r\nX-Kept: 1\r\n\r\n"
        );
    }

    #[test]
    fn test_close_response_head_drops_a_dropped_headers_continuation_only() {
        assert_eq!(
            closed("HTTP/1.1 200 OK\r\nConnection: Upgrade,\r\n keep-alive\r\nX-Long: a,\r\n\tb\r\n\r\n"),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nX-Long: a,\r\n\tb\r\n\r\n"
        );
    }

    /// Chrome would join a first header line that starts with whitespace
    /// onto the `Connection: close` before it (SME-165 review 1).
    #[test]
    fn test_close_response_head_drops_a_leading_continuation_line() {
        assert_eq!(
            closed("HTTP/1.1 200 OK\r\n X-Foo: bar\r\n\tmore\r\nContent-Length: 2\r\n\r\n"),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\n"
        );
    }

    #[test]
    fn test_close_response_head_adds_close_when_there_was_none() {
        assert_eq!(
            closed("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n"),
            "HTTP/1.0 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        );
        assert_eq!(closed("HTTP/1.1 204 No Content\r\n\r\n"), "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
    }

    /// Chrome ends a header line at any CR or LF, and drops NUL bytes
    /// before reading names (`HttpUtil::AssembleRawHeaders`).
    #[test]
    fn test_close_response_head_reads_lines_and_names_as_chrome_does() {
        assert_eq!(
            closed("HTTP/1.1 200 OK\nX-A: 1\rConnection: keep-alive\r\nProxy-Conn\0ection: keep-alive\n\n"),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nX-A: 1\r\n\r\n"
        );
    }

    /// `HttpResponseHeaders::ParseStatusLine` and `ParseStatus`: the code
    /// starts after the line's first space, past more spaces (not tabs);
    /// no digits there means 200.
    #[test]
    fn test_status_code_reads_as_chrome_does() {
        for (line, code) in [
            ("HTTP/1.1 100 Continue\r\n\r\n", 100),
            ("http/1.1   0103 Early Hints\r\n\r\n", 103),
            ("HTTP/1.1 1\u{0}01 Switching\r\n\r\n", 101),
            ("HTTP/1.1 204\r\n\r\n", 204),
            ("HTTP/1.1\r\n\r\n", 200),
            ("HTTP/1.1 OK\r\n\r\n", 200),
            ("HTTP/1.1\t100 Continue\r\n\r\n", 200),
            ("HTTP/1.1 100000000000000000000000 Big\r\n\r\n", u64::MAX),
        ] {
            assert_eq!(status_code(line.as_bytes()), code, "{line:?}");
        }
    }
}
