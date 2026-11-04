//! `smelt-mcp`, the scripts' client for the MCP socket (SME-359): a link
//! to the agent binary in the image, so it ships with the agent it talks
//! to and needs nothing the image doesn't have.
//!
//! ```text
//! smelt-mcp tools [server]
//! smelt-mcp call <server>/<tool> [<json arguments> | -]
//! ```
//!
//! The answer's JSON body goes to stdout whatever the status. Exit 0 for a
//! 2xx, 1 for the tool's own error (422), 2 for a refusal or a limit (403,
//! 429), 3 for anything else, a usage error included.

use std::io::{Read, Write};

use super::mcp::MCP_SOCKET_ENV;

/// The name the link has in the image.
pub(super) const CLI_NAME: &str = "smelt-mcp";

const USAGE: &str = "usage: smelt-mcp tools [server]\n       smelt-mcp call <server>/<tool> [<json arguments> | -]\n\
     Calls this conversation's MCP tools through smelt; the answer's JSON is printed on stdout.\n\
     Exit codes: 0 ok, 1 the tool reported an error, 2 refused or over a limit, 3 anything else.";

pub(super) fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket = std::env::var(MCP_SOCKET_ENV).ok();
    let mut stdin = std::io::stdin();
    let (code, out, err) = run(&args, socket.as_deref(), super::mcp::MCP_SOCKET_PATH, &mut stdin);
    let _ = std::io::stdout().write_all(out.as_bytes());
    if !out.is_empty() && !out.ends_with('\n') {
        println!();
    }
    if !err.is_empty() {
        eprintln!("smelt-mcp: {err}");
    }
    std::process::ExitCode::from(code)
}

/// One run: the exit code, stdout and stderr. `socket` is
/// `$SMELT_MCP_SOCKET`; without it (`sudo` and `env -i` drop it) the
/// socket is `fallback`, its fixed path, when that exists (SME-386 M7).
pub(super) fn run(args: &[String], socket: Option<&str>, fallback: &str, stdin: &mut dyn Read) -> (u8, String, String) {
    let request = match args.first().map(String::as_str) {
        Some("tools") if args.len() <= 2 => {
            let path = match args.get(1) {
                Some(server) => format!("/v1/tools?server={}", percent_encode(server)),
                None => "/v1/tools".to_string(),
            };
            format!("GET {path} HTTP/1.1\r\nHost: smelt\r\nConnection: close\r\n\r\n")
        }
        Some("call") if (2..=3).contains(&args.len()) => {
            let arguments = match args.get(2).map(String::as_str) {
                None => "{}".to_string(),
                Some("-") => {
                    let mut text = String::new();
                    if let Err(e) = stdin.read_to_string(&mut text) {
                        return (3, String::new(), format!("couldn't read the arguments from stdin: {e}"));
                    }
                    text
                }
                Some(text) => text.to_string(),
            };
            // Read here, so arguments that aren't an object are a usage
            // error of the CLI's own (SME-386 M8): spliced in as they were,
            // `''` made a body smelt could only call malformed. They go up
            // as the script wrote them, not re-serialized.
            match serde_json::from_str::<serde_json::Value>(&arguments) {
                Ok(serde_json::Value::Object(_)) => {}
                Ok(other) => {
                    let kind = match other {
                        serde_json::Value::Array(_) => "an array",
                        serde_json::Value::String(_) => "a string",
                        serde_json::Value::Number(_) => "a number",
                        serde_json::Value::Bool(_) => "a boolean",
                        _ => "null",
                    };
                    return (3, String::new(), format!("the arguments must be a JSON object, like '{{\"query\":\"x\"}}', not {kind}"));
                }
                Err(e) => {
                    return (3, String::new(), format!("the arguments aren't JSON ({e}); pass an object, like '{{\"query\":\"x\"}}', or none for {{}}"));
                }
            }
            let body = format!("{{\"tool\":{},\"arguments\":{}}}", json_string(&args[1]), arguments.trim());
            format!(
                "POST /v1/call HTTP/1.1\r\nHost: smelt\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        _ => return (3, String::new(), USAGE.to_string()),
    };
    // After the request is built, so a usage or arguments error is said
    // first (review round 1, N2).
    let socket = match socket.filter(|s| !s.is_empty()) {
        Some(socket) => socket,
        None if std::path::Path::new(fallback).exists() => fallback,
        None => {
            return (
                3,
                String::new(),
                format!(
                    "${MCP_SOCKET_ENV} isn't set and there's no socket at {fallback}: this sandbox's image predates script MCP calls, \
                     or its agent couldn't open the socket; stop and start the sandbox to get one that has them"
                ),
            );
        }
    };
    match exchange(socket, &request) {
        Ok((status, body)) => (exit_code(status), body, String::new()),
        Err(e) => (3, String::new(), format!("couldn't reach the MCP socket at {socket}: {e}")),
    }
}

fn exit_code(status: u16) -> u8 {
    match status {
        200..=299 => 0,
        422 => 1,
        403 | 429 => 2,
        _ => 3,
    }
}

fn json_string(text: &str) -> String {
    serde_json::Value::from(text).to_string()
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Sends `request` and reads the whole answer: its status and body.
fn exchange(socket: &str, request: &str) -> std::io::Result<(u16, String)> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(request.as_bytes())?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer)?;
    parse_response(&answer).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "not an HTTP answer"))
}

/// An HTTP/1.1 answer's status and body, a chunked one included.
pub(super) fn parse_response(answer: &[u8]) -> Option<(u16, String)> {
    let split = answer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&answer[..split]).ok()?;
    let body = &answer[split + 4..];
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = if chunked { dechunk(body)? } else { body.to_vec() };
    Some((status, String::from_utf8_lossy(&body).into_owned()))
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}
