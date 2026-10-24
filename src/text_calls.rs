//! Tool calls a model wrote as text instead of making them (SME-286).
//!
//! A provider that doesn't parse a call the model writes (an unknown
//! tool's name, a looser parser than llama.cpp's) hands it back as
//! ordinary text. Saved as a reply, it ended the turn with no error, and
//! the model went on as if the call had run. The turn loop finds such
//! calls in a reply's text blocks (`find`), never its thinking, and tells
//! the model (`text_call_notice`), naming offered tools with similar names
//! (`similar`). The transcript reads the names back out of the notice
//! (`parse_text_call_notice`), so the notice and its parse live together.
//!
//! Detection is hand-scanned, form by form, for the markup llama.cpp's chat
//! templates produce (Qwen3-Coder, Hermes, GLM, MiniMax, Seed-OSS, Mistral,
//! DeepSeek V3.1 and V3.2, Kimi K2, Gemma 4, LFM2, gpt-oss, Llama 3.1, and
//! Claude's own `<function_calls>`). Code is masked first: fenced and
//! inline code, indented lines and blockquotes. Special-token markers count
//! anywhere else; tag markers, which prose can quote, count only at a
//! line's start and only with the structure that names a tool. Not
//! detected: a bare JSON object (a model printing JSON would match it),
//! Functionary v3.2's `>>>NAME`, and any form not listed here.

/// A tool call found written in a reply's text: its tool's name, when the
/// form has one to read and it reads as a name.
#[cfg(feature = "server")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCall {
    pub name: Option<String>,
}

/// How a text-call notice starts (`text_call_notice`).
pub const TEXT_CALL_NOTICE_START: &str =
    "Your last reply wrote a tool call as text instead of making it, so it didn't run.";

/// How a text-call notice ends.
const TEXT_CALL_NOTICE_END: &str = " Make the call as a tool call, not as text in your reply.";

/// A text-call notice's sentence for a call whose name couldn't be read.
const UNREADABLE: &str = "One call's tool name couldn't be read";

/// Saved after a reply with no text and no tool call (thinking only, or
/// empty), once a turn: the model carries on rather than the turn ending
/// silently.
pub const EMPTY_REPLY_NOTICE: &str =
    "Your last reply had no text and no tool call. Carry on: make the tool call you need, or answer.";

/// How many corrections in a row a turn sends before the next reply with a
/// call written as text ends it with an error.
#[cfg(feature = "server")]
pub const MAX_TEXT_CALL_CORRECTIONS: usize = 3;

/// The longest tool name read; a longer one is unreadable.
const MAX_NAME: usize = 128;

/// Whether `c` can be part of a tool name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

/// The name characters `s` starts with.
#[cfg(feature = "server")]
fn name_at(s: &str) -> &str {
    let end = s.find(|c: char| !is_name_char(c)).unwrap_or(s.len());
    &s[..end]
}

/// `name`, if it reads as a tool name: 1 to 128 name characters.
fn checked(name: &str) -> Option<String> {
    let ok = !name.is_empty() && name.len() <= MAX_NAME && name.chars().all(is_name_char);
    ok.then(|| name.to_string())
}

/// The tool calls written in `text`, a reply's text block, in order.
#[cfg(feature = "server")]
pub fn find(text: &str) -> Vec<TextCall> {
    let masked = mask_code(text);
    let mut found: Vec<(usize, TextCall)> = Vec::new();
    // A tag call's parse can run onto later lines (`<function_calls>`, then
    // its first `<invoke>`); a line it reached isn't read again.
    let mut consumed = 0;
    let mut start = 0;
    for line in masked.split_inclusive('\n') {
        let end = start + line.len();
        for (marker, read) in SPECIAL_MARKERS {
            for (at, _) in line.match_indices(marker) {
                let after = start + at + marker.len();
                if let Some(call) = read(&text[after..]) {
                    found.push((start + at, call));
                }
            }
        }
        // The indent of the line as written: a code span blanked at its
        // start isn't indent (review 1).
        let written = &text[start..end];
        let indent = written.len() - written.trim_start_matches(' ').len();
        if start >= consumed && indent <= 3 && line[indent..].starts_with('<') {
            let at = start + indent;
            if let Some((call, used)) = tag_call(&text[at..]) {
                found.push((at, call));
                consumed = at + used;
            }
        }
        start = end;
    }
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, call)| call).collect()
}

/// A special-token marker, and how the call after it reads: `None` when
/// what follows isn't a call of its own (a section marker followed by its
/// call marker, which counts instead).
#[cfg(feature = "server")]
type ReadAfter = fn(&str) -> Option<TextCall>;

/// Markers that never occur in prose, and count anywhere outside code, each
/// only with its form's structure after it, so prose naming a marker isn't
/// a call (review 1). A section marker (Kimi's, DeepSeek's) counts through
/// the call marker inside it.
#[cfg(feature = "server")]
const SPECIAL_MARKERS: &[(&str, ReadAfter)] = &[
    // Mistral: `[TOOL_CALLS]NAME[ARGS]{…}`, or a JSON list with no name to read.
    ("[TOOL_CALLS]", |s| {
        let name = name_at(s);
        let rest = &s[name.len()..];
        if !name.is_empty() && (rest.starts_with("[ARGS]") || rest.starts_with("[CALL_ID]")) {
            Some(named(name))
        } else {
            (s.starts_with("[{") || s.starts_with('{')).then_some(TextCall { name: None })
        }
    }),
    // Kimi K2: `<|tool_call_begin|>functions.NAME:0<|tool_call_argument_begin|>`.
    ("<|tool_call_begin|>", |s| {
        let id = name_at(s);
        s[id.len()..].starts_with("<|tool_call_argument_begin|>").then(|| {
            let id = id.strip_prefix("functions.").unwrap_or(id);
            let name = id.rsplit_once(':').filter(|(_, n)| n.chars().all(|c| c.is_ascii_digit())).map_or(id, |(name, _)| name);
            named(name)
        })
    }),
    // DeepSeek V3.1: `<｜tool▁call▁begin｜>NAME<｜tool▁sep｜>` (R1's
    // `function<｜tool▁sep｜>NAME` too).
    ("<｜tool▁call▁begin｜>", |s| {
        let name = name_at(s);
        let rest = s[name.len()..].strip_prefix("<｜tool▁sep｜>")?;
        Some(named(if name == "function" { name_at(rest) } else { name }))
    }),
    // Gemma 4: `<|tool_call>call:NAME{…}`.
    ("<|tool_call>call:", |s| {
        let name = name_at(s);
        s[name.len()..].starts_with('{').then(|| named(name))
    }),
    // LFM2: `<|tool_call_start|>[NAME(…)]`.
    ("<|tool_call_start|>", |s| {
        let s = s.strip_prefix('[')?;
        let name = name_at(s);
        s[name.len()..].starts_with('(').then(|| named(name))
    }),
    // gpt-oss: `to=functions.NAME` in a header, then its next token.
    ("to=functions.", |s| {
        let name = name_at(s);
        let rest = &s[name.len()..];
        (rest.starts_with("<|") || rest.starts_with(" <|")).then(|| named(name))
    }),
];

#[cfg(feature = "server")]
fn named(name: &str) -> TextCall {
    TextCall { name: checked(name) }
}

/// The local name of the opening tag `s` starts with (what follows `<` and
/// any namespace: `minimax:`, `seed:`, `｜DSML｜`), or `None` if `s` doesn't
/// start with an opening tag.
#[cfg(feature = "server")]
fn tag(s: &str) -> Option<&str> {
    let s = s.strip_prefix('<')?;
    if s.starts_with('/') {
        return None;
    }
    let bare = s.strip_prefix('｜').unwrap_or(s);
    let ns = bare.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(bare.len());
    let after = &bare[ns..];
    let local = after.strip_prefix(':').or_else(|| after.strip_prefix('｜'));
    Some(match local {
        Some(local) if ns > 0 => local,
        _ => s,
    })
}

/// A closing tag for `local` (`</invoke>`, `</｜DSML｜invoke>`) at `s`'s start.
#[cfg(feature = "server")]
fn is_close(s: &str, local: &str) -> bool {
    s.strip_prefix("</").is_some_and(|rest| tag(&format!("<{rest}")).is_some_and(|l| l.starts_with(local)))
}

/// Byte offset of `inner`, a subslice, in `outer`.
#[cfg(feature = "server")]
fn offset(outer: &str, inner: &str) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

/// A call written with tags, at `s`'s start (a line's), and how far into
/// `s` its parse read.
#[cfg(feature = "server")]
fn tag_call(s: &str) -> Option<(TextCall, usize)> {
    let local = tag(s)?;
    let done = |rest: &str, call: TextCall| Some((call, offset(s, rest)));
    if let Some(after) = local.strip_prefix("tool_call>") {
        // GLM: `<tool_call>NAME`, then a newline, `<arg_key>` or the end.
        let glm = name_at(after);
        let tail = &after[glm.len()..];
        if !glm.is_empty() && ["\n", "\r\n", "<arg_key>", "</tool_call>"].iter().any(|t| tail.starts_with(t)) {
            return done(tail, named(glm));
        }
        let inner = after.trim_start();
        if inner.starts_with('{') {
            // Hermes: a JSON object with a string `"name"`.
            return json_name(inner).map(|name| (TextCall { name }, offset(s, inner) + 1));
        }
        // Qwen3-Coder, Seed-OSS: `<function=NAME>`; MiniMax: `<invoke name="NAME">`.
        let inner_local = tag(inner)?;
        if let Some(rest) = inner_local.strip_prefix("function=") {
            let name = name_at(rest);
            return done(&rest[name.len()..], named(name));
        }
        return invoke(s, inner_local);
    }
    if let Some(after) = local.strip_prefix("function_calls>") {
        // Claude, DeepSeek V3.2: `<function_calls>`, then `<invoke name="NAME">`.
        return invoke(s, tag(after.trim_start())?);
    }
    if let Some(rest) = local.strip_prefix("function=") {
        // Bare Qwen3-Coder, Llama 3.1: `<function=NAME>`, then its arguments.
        let name = name_at(rest);
        let body = rest[name.len()..].strip_prefix('>')?.trim_start();
        return (body.starts_with("<parameter=") || body.starts_with('{')).then(|| (named(name), offset(s, body)));
    }
    if local.starts_with("invoke name=\"") {
        // An `<invoke>` on its own line: its parameters or its end follow.
        let (call, used) = invoke(s, local)?;
        let body = s[used..].split_once('>')?.1.trim_start();
        let param = tag(body).is_some_and(|l| l.starts_with("parameter name="));
        return (param || is_close(body, "invoke>")).then_some((call, used));
    }
    None
}

/// `<invoke name="NAME">` (`local` is the tag past its `<` and namespace).
#[cfg(feature = "server")]
fn invoke<'a>(s: &'a str, local: &'a str) -> Option<(TextCall, usize)> {
    let rest = local.strip_prefix("invoke name=\"")?;
    let (name, _) = rest.split_once('"')?;
    Some((TextCall { name: checked(name) }, offset(s, rest) + name.len()))
}

/// A JSON call's `"name"`: `Some(None)` when it has one that isn't a
/// name, `None` when it isn't a call (no string `"name"`).
#[cfg(feature = "server")]
fn json_name(s: &str) -> Option<Option<String>> {
    let parsed = serde_json::Deserializer::from_str(s).into_iter::<serde_json::Value>().next();
    if let Some(Ok(value)) = parsed {
        return value.get("name")?.as_str().map(checked);
    }
    // Cut short (a reply that stopped mid-call): its `"name"` still reads,
    // from this call only, never from text after its end (review 1).
    let call = s.split_once("</tool_call>").map_or(s, |(call, _)| call);
    let (_, rest) = call.split_once("\"name\"")?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(checked(rest.split_once('"')?.0))
}

/// `text` with its code blanked (spaces, newlines kept, same byte offsets):
/// fenced blocks (an unclosed fence runs to the end), inline code spans
/// (within a paragraph, so one can span a line break; a whole tag call at a
/// line's start ends the paragraph), lines indented 4 or more, blockquote
/// lines, and reasoning leaked into the text: a `<think>` at a line's start
/// up to the `</think>` that closes it (reviews 1 and 2).
#[cfg(feature = "server")]
fn mask_code(text: &str) -> String {
    let mut out = text.as_bytes().to_vec();
    let mut blank = |from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    // Leaked reasoning: a `<think>` at a line's start, up to the `</think>`
    // that closes it. One elsewhere (prose, inline code) or never closed is
    // text, so it can't hide the rest of the reply (review 2).
    let mut from = 0;
    while from < text.len() {
        let line_end = text[from..].find('\n').map_or(text.len(), |at| from + at + 1);
        let line = &text[from..line_end];
        let body = line.trim_start_matches(' ');
        if line.len() - body.len() <= 3
            && body.starts_with("<think>")
            && let Some(at) = text[line_end - body.len() + "<think>".len()..].find("</think>")
        {
            let open = line_end - body.len();
            let close = open + "<think>".len() + at + "</think>".len();
            blank(open, close);
            from = text[close..].find('\n').map_or(text.len(), |at| close + at + 1);
            continue;
        }
        from = line_end;
    }
    let mut fence: Option<(char, usize)> = None;
    // The plain lines since the last blank or code line: a paragraph, whose
    // code spans are blanked once it ends.
    let mut paragraph: Option<usize> = None;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        let indent: usize = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum();
        let body = line.trim_start_matches([' ', '\t']);
        let run = |ch: char| body.chars().take_while(|c| *c == ch).count();
        // A complete call at a line's start ends the paragraph: a code span
        // opened by a stray backtick before it can't run into it (review 2).
        let call = fence.is_none() && indent <= 3 && complete_tag_call(&text[end - body.len()..]);
        let plain = fence.is_none() && indent <= 3 && run('`') < 3 && run('~') < 3 && !body.starts_with('>') && !body.trim().is_empty() && !call;
        if !plain && let Some(para) = paragraph.take() {
            for (from, to) in code_spans(&text[para..start]) {
                blank(para + from, para + to);
            }
        }
        if let Some((ch, len)) = fence {
            blank(start, end);
            if indent <= 3 && run(ch) >= len && body.trim_start_matches(ch).trim().is_empty() {
                fence = None;
            }
        } else if indent <= 3 && (run('`') >= 3 || run('~') >= 3) {
            let ch = if run('`') >= 3 { '`' } else { '~' };
            fence = Some((ch, run(ch)));
            blank(start, end);
        } else if indent >= 4 || body.starts_with('>') {
            blank(start, end);
        } else if plain && paragraph.is_none() {
            paragraph = Some(start);
        }
        start = end;
    }
    if let Some(para) = paragraph {
        for (from, to) in code_spans(&text[para..]) {
            blank(para + from, para + to);
        }
    }
    // Each blanked range is whole characters (whole lines, code spans that
    // start and end at a backtick, `<think>` to `</think>`), every byte of it
    // made a space.
    #[expect(clippy::expect_used, reason = "only whole characters were replaced, each byte with an ASCII space")]
    let masked = String::from_utf8(out).expect("only whole characters were replaced, with spaces");
    masked
}

/// Whether `s` starts with a call written with tags that is closed on a
/// later line's start (`</tool_call>`, `</function>`, `</function_calls>`,
/// `</invoke>`): a whole call, not a fragment quoted in prose.
#[cfg(feature = "server")]
fn complete_tag_call(s: &str) -> bool {
    s.starts_with('<')
        && tag_call(s).is_some()
        && s.lines().skip(1).any(|l| {
            let l = l.trim_start();
            ["tool_call>", "function>", "function_calls>", "invoke>"].iter().any(|local| is_close(l, local))
        })
}

/// The inline code spans in `text` (a paragraph), as byte ranges, backticks
/// included: a run of backticks up to the next run of the same length.
#[cfg(feature = "server")]
fn code_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let run_at = |i: usize| bytes[i..].iter().take_while(|b| **b == b'`').count();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open = run_at(i);
        let mut j = i + open;
        let mut close = None;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let n = run_at(j);
                if n == open {
                    close = Some(j + n);
                    break;
                }
                j += n;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                spans.push((i, end));
                i = end;
            }
            None => i += open,
        }
    }
    spans
}

/// Verbs that name the same kind of action, each mapped to one word, so
/// `add_issue_comment` meets `save_comment` and `issue_read` `get_issue`.
#[cfg(feature = "server")]
const VERBS: &[(&str, &[&str])] = &[
    ("save", &["add", "create", "save", "post", "write", "new", "insert", "put", "set", "update", "edit", "upsert", "make", "submit"]),
    ("get", &["get", "read", "fetch", "view", "show", "retrieve", "load", "describe", "open"]),
    ("list", &["list", "search", "find", "query", "browse"]),
    ("delete", &["delete", "remove", "rm", "del", "destroy", "drop"]),
];

/// A tool name's MCP server (`mcp__<server>__`), if any, and its words:
/// lowercased, split on `_`, `-` and `.`, a plural's `s` dropped, verbs
/// mapped to one word each.
#[cfg(feature = "server")]
fn server_and_words(name: &str) -> (Option<&str>, std::collections::BTreeSet<String>) {
    let (server, tool) = crate::mcp::parse_tool_name(name).map_or((None, name), |(s, t)| (Some(s), t));
    let words = tool
        .split(['_', '-', '.'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_ascii_lowercase();
            let w = if w.len() > 3 && w.ends_with('s') { w[..w.len() - 1].to_string() } else { w };
            VERBS.iter().find(|(_, all)| all.contains(&w.as_str())).map_or(w, |(verb, _)| verb.to_string())
        })
        .collect();
    (server, words)
}

/// Whether two MCP servers' names are the same server's (case aside).
#[cfg(feature = "server")]
fn same_server(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    }
}

/// Offered tools whose names are like `name`'s, best first, at most 5.
/// A candidate shares at least one word with `name` other than a verb.
/// The ones on `name`'s own MCP server come first; then the more (and
/// rarer, among the offered tools) words shared, the better; then the
/// fewer words of its own, then by name.
#[cfg(feature = "server")]
pub fn similar(name: &str, offered: &[&str]) -> Vec<String> {
    let (server, words) = server_and_words(name);
    let candidates: Vec<(&str, Option<&str>, std::collections::BTreeSet<String>)> = offered
        .iter()
        .filter(|o| **o != name)
        .map(|o| {
            let (s, w) = server_and_words(o);
            (*o, s, w)
        })
        .collect();
    let weight = |word: &String| {
        let df = candidates.iter().filter(|(_, _, w)| w.contains(word)).count().max(1);
        1 + (offered.len().max(1) / df).max(1).ilog2() as usize
    };
    let mut scored: Vec<(bool, usize, usize, &str)> = candidates
        .iter()
        .filter_map(|(o, s, w)| {
            let shared: Vec<&String> = words.intersection(w).collect();
            if !shared.iter().any(|word| !VERBS.iter().any(|(verb, _)| verb == word)) {
                return None;
            }
            let score = shared.iter().map(|word| weight(word)).sum();
            Some((same_server(server, *s), score, w.len() - shared.len(), *o))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(b.3)));
    scored.into_iter().take(5).map(|(_, _, _, o)| o.to_string()).collect()
}

/// The notice saved after a reply that wrote `calls` as text: each call by
/// name, whether it's offered, and the offered tools with similar names.
/// It quotes only tool names, never a call's parameters.
#[cfg(feature = "server")]
pub fn text_call_notice(calls: &[TextCall], offered: &[&str]) -> String {
    let quote = |names: &[String]| names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
    let mut notice = TEXT_CALL_NOTICE_START.to_string();
    for call in calls {
        let Some(name) = &call.name else {
            notice.push_str(&format!(" {UNREADABLE}."));
            continue;
        };
        if offered.contains(&name.as_str()) {
            notice.push_str(&format!(" `{name}` is offered, but a call written in a reply's text isn't made."));
            continue;
        }
        let missing_server = crate::mcp::parse_tool_name(name)
            .map(|(server, _)| server)
            .filter(|server| !offered.iter().any(|o| crate::mcp::parse_tool_name(o).is_some_and(|(s, _)| s.eq_ignore_ascii_case(server))));
        let suggestions = similar(name, offered);
        let mut sentence = format!(" `{name}` isn't an offered tool");
        if let Some(server) = missing_server {
            sentence.push_str(&format!(", and no MCP server `{server}` is offered"));
        }
        match (suggestions.is_empty(), missing_server.is_some()) {
            (false, _) => sentence.push_str(&format!("; similar offered tools: {}.", quote(&suggestions))),
            (true, true) => sentence.push('.'),
            (true, false) => sentence.push_str(", and no offered tool has a similar name."),
        }
        notice.push_str(&sentence);
    }
    notice.push_str(TEXT_CALL_NOTICE_END);
    notice
}

/// The turn's error once the model has written calls as text more times
/// in a row than it's corrected for: `calls` are the last reply's.
#[cfg(feature = "server")]
pub fn bound_error(calls: &[TextCall]) -> String {
    let last = match calls.iter().find_map(|c| c.name.as_deref()) {
        Some(name) => format!("the last: `{name}`"),
        None => "the last one's tool name couldn't be read".to_string(),
    };
    format!(
        "The model wrote a tool call as text instead of making it {} times in a row ({last}), so the turn stopped.",
        MAX_TEXT_CALL_CORRECTIONS + 1
    )
}

/// A text-call notice's calls, by name (`None` for one whose name couldn't
/// be read), for the transcript; `None` for any other text.
pub fn parse_text_call_notice(text: &str) -> Option<Vec<Option<String>>> {
    let body = text.strip_prefix(TEXT_CALL_NOTICE_START)?.strip_suffix(TEXT_CALL_NOTICE_END)?;
    let body = body.strip_prefix(' ')?.strip_suffix('.')?;
    body.split(". ")
        .map(|sentence| {
            if sentence == UNREADABLE {
                return Some(None);
            }
            let (name, tail) = sentence.strip_prefix('`')?.split_once('`')?;
            let known = tail.starts_with(" is offered") || tail.starts_with(" isn't an offered tool");
            known.then(|| checked(name)).flatten().map(Some)
        })
        .collect()
}

#[cfg(all(test, feature = "server"))]
mod tests;
