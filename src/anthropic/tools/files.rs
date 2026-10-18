//! Files in the sandbox: read, write, edit, list, glob, grep.

use serde_json::Value;
use sqlx::PgPool;

use super::Tool;
use super::pods::{LINES_BUDGET_CHARS, MAX_LINE_CHARS, fit_lines};
use super::server::required_str;
use crate::anthropic::ContentBlock;
use crate::models::Message;
use crate::{db, sandbox};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        // --- File tools: read/edit/write a file, and list a
        // directory, in this conversation's sandbox pod — see
        // SME-11.
        Tool {
            def: ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file from this conversation's sandbox pod. Paginated \
                               (offset/limit, line-based) so a huge file doesn't have to be \
                               consumed into context all at once. Returns line-numbered \
                               content, the file's total line count, and a content hash — \
                               edit_file, and write_file when overwriting, need that hash \
                               (as expected_hash) to confirm nothing else changed the file \
                               first. Lines over 2000 characters are cut, ending in a note \
                               such as [line 7 cut: characters 1–2000 of 40000 shown; read \
                               on with offset 7, character 2001]; pass that offset and \
                               character to read on within the line. character (1-based, \
                               counted as lsp counts it) starts the read at that character \
                               instead of the line's start and returns that line alone, and \
                               the reply then says character. next_offset, when present, is \
                               where the next whole line starts (truncated: true means this \
                               read stopped early for size, not at limit)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "minimum": 1, "description": "1-indexed starting line, defaults to 1"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "defaults to 2000, capped at 2000"},
                        "character": {"type": "integer", "minimum": 1, "description": "1-based character in the offset line to start at, defaults to 1; use the one a cut line's note gives"}
                    },
                    "required": ["path"]
                }),
            },
            run: run!(|c| read_file_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "edit_file".to_string(),
                description: "Make a targeted old_string -> new_string replacement in a file \
                               in this conversation's sandbox pod — far cheaper and less risky \
                               than rewriting the whole file. Both strings can span multiple \
                               lines. old_string must match exactly once in the file, or the \
                               call fails (asking for more surrounding context to disambiguate) \
                               unless replace_all is set. For truly identical repeated blocks \
                               that no amount of extra context can disambiguate, set \
                               expected_line (the line old_string starts at, from read_file's \
                               line-numbered output) to target that one occurrence directly \
                               instead — mutually exclusive with replace_all. Requires that \
                               this exact path was already read_file'd (or written/edited) \
                               earlier in this conversation, and refuses if the file has \
                               changed since then — call read_file (again) first."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean", "description": "defaults to false"},
                        "expected_line": {"type": "integer", "minimum": 1, "description": "target one specific occurrence by its starting line; mutually exclusive with replace_all"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            run: run!(|c| edit_file_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "write_file".to_string(),
                description: "Create a new file, or fully overwrite an existing one, in this \
                               conversation's sandbox pod. Overwriting a path this conversation \
                               already read_file'd (or wrote/edited) checks that it hasn't \
                               changed since, and refuses if it has — call read_file first. \
                               Creating a brand-new path needs no prior read."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            },
            run: run!(|c| write_file_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "list_directory".to_string(),
                description: "List a directory's contents (one level, not recursive) in this \
                               conversation's sandbox pod — each entry's name, whether it's a \
                               file or directory, and byte size for files."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
            run: run!(|c| list_directory_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "glob".to_string(),
                description: "Find files under path whose path relative to path matches a glob \
                               pattern, in this conversation's sandbox pod — e.g. **/*.rs for \
                               every .rs file at any depth, *.rs for only ones directly in path \
                               (a bare * never crosses a directory separator; only ** does). \
                               Paginated (offset/limit) — response also carries total (how many \
                               matches were found, up to an internal scan limit) and \
                               scan_capped (true if that internal limit, not just this page, was \
                               hit; narrow pattern or path rather than just paging further when \
                               that happens)."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "root directory to search from"},
                        "pattern": {"type": "string", "description": "glob pattern relative to path, e.g. **/*.rs"},
                        "offset": {"type": "integer", "minimum": 1, "description": "1-indexed starting result, defaults to 1"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "defaults to 50, capped at 200"}
                    },
                    "required": ["path", "pattern"]
                }),
            },
            run: run!(|c| glob_tool(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "grep".to_string(),
                description: "Search file contents under path for a regex pattern, in this \
                               conversation's sandbox pod. Returns each match's file, 1-indexed \
                               line number, and line text. Optionally narrow to files matching a \
                               glob first (same pattern syntax as the glob tool). A file that's \
                               skipped (not valid UTF-8, or too large) is named, with why, in \
                               skipped — never silently omitted. Paginated (offset/limit) — see \
                               the glob tool's description for what total/scan_capped mean."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "root directory to search from"},
                        "pattern": {"type": "string", "description": "regex to search file contents for"},
                        "glob": {"type": "string", "description": "optional glob filter, e.g. *.rs — only search matching files; defaults to every file"},
                        "case_insensitive": {"type": "boolean", "description": "defaults to false"},
                        "offset": {"type": "integer", "minimum": 1, "description": "1-indexed starting result, defaults to 1"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "defaults to 20, capped at 100"}
                    },
                    "required": ["path", "pattern"]
                }),
            },
            run: run!(|c| grep_tool(c.pool, c.conversation_id, c.input)),
        },
    ]
}

/// The hash from the *most recent* successful `read_file`/`write_file`/
/// `edit_file` result for `path` in `messages`, if any — what
/// `edit_file`/`write_file`'s wrapper functions check for before ever
/// contacting the agent (the "read-before-write discipline"). `messages`
/// is expected in chronological order (as `db::list_messages` already
/// returns it), so a later match naturally overrides an earlier one.
pub(super) fn find_prior_file_hash(messages: &[Message], path: &str) -> Option<String> {
    const FILE_TOOLS: &[&str] = &["read_file", "write_file", "edit_file"];

    let mut relevant_tool_use_ids = std::collections::HashSet::new();
    let mut latest_hash = None;

    for message in messages {
        let Ok(blocks) = message.blocks() else {
            continue;
        };
        for block in blocks {
            match block {
                ContentBlock::ToolUse { id, name, input }
                    if FILE_TOOLS.contains(&name.as_str())
                        && input.get("path").and_then(Value::as_str) == Some(path) =>
                {
                    relevant_tool_use_ids.insert(id);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if relevant_tool_use_ids.contains(&tool_use_id)
                    && !is_error.unwrap_or(false) =>
                {
                    if let Ok(value) = serde_json::from_str::<Value>(&content) {
                        if let Some(hash) = value.get("hash").and_then(Value::as_str) {
                            latest_hash = Some(hash.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    latest_hash
}

const DEFAULT_READ_FILE_LIMIT: u32 = 2000;

const MAX_READ_FILE_LIMIT: u32 = 2000;

/// `read_file`'s `character` input: the 1-based character of the `offset`
/// line to start the read at, defaulting to the line's start. Held to what a
/// position can be *before* the cast, so a number too big for one falls into
/// `read_file_response`'s past-the-end refusal instead of wrapping round to
/// a real position and answering the wrong characters under a position that
/// looks self-consistent (SME-189 review 1).
fn character_param(input: &Value) -> u32 {
    input
        .get("character")
        .and_then(Value::as_u64)
        .map(|c| c.clamp(1, u32::MAX as u64))
        .unwrap_or(1) as u32
}

async fn read_file_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as u32;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_FILE_LIMIT as u64)
        .clamp(1, MAX_READ_FILE_LIMIT as u64) as u32;
    let character = character_param(input);
    // A read that starts mid-line answers one line, so don't ask the pod for
    // the lines after it: walking a long line would otherwise fetch (and a
    // refusal would fetch) a whole page each step (SME-189 review 1).
    let limit = if character > 1 { 1 } else { limit };

    let contents = sandbox::read_file(pool, conversation_id, &path, offset, limit)
        .await
        .map_err(|e| e.to_string())?;
    Ok(read_file_response(contents, offset, character)?.to_string())
}

/// `read_file`'s answer: `contents`' lines numbered from `offset` — the
/// first starting at `character`, a 1-based character of that line, when it
/// is more than 1 — as many as fit `LINES_BUDGET_CHARS` (`truncated` when
/// some didn't), with `next_offset` whenever the file goes on past them.
///
/// A line longer than `MAX_LINE_CHARS` is cut by `cut_line`, and the note
/// that says so is the only way on into it. `next_offset` stays the next
/// *whole* line and `truncated` stays "lines left out for the budget", so a
/// reader that ignores `character` still pages by whole lines and can't loop
/// on one (SME-189). A read that starts mid-line answers that line alone —
/// its page holds no other line — so following a long line's notes costs the
/// line rather than the file (SME-189 review 1). `hash` is always the whole
/// file's, whatever was cut, so an `edit_file` after a partial read checks
/// against the same hash.
pub(super) fn read_file_response(
    contents: crate::agent_protocol::FileContents,
    offset: u32,
    character: u32,
) -> Result<Value, String> {
    // the line number, a tab and the joining newline, JSON-escaped
    const PER_LINE: usize = 10;
    let mut lines = contents.lines;
    if character > 1 {
        if let Some(first) = lines.first_mut() {
            let length = first.chars().count();
            let start = character as usize;
            if start > length {
                return Err(format!(
                    "character {start} is past the end of line {offset}, which has {length} characters"
                ));
            }
            *first = first.chars().skip(start - 1).collect();
        }
        // A read that starts mid-line is following one line's note, so it
        // answers that line and nothing else: the lines after it belong to
        // the next whole line, and re-sending them on every step of a walk
        // costs about twelve times what the walk wins (SME-189 review 1).
        lines.truncate(1);
    }
    // Each line cut for itself, so `fit_lines` below never cuts one: it
    // sees texts of at most `MAX_LINE_CHARS` characters. The notes go on
    // after it, which is what keeps `pods::fit_lines` unchanged — a cut line
    // costs at least 2,010 of the budget, so at most 12 of them fit a read,
    // and their notes fit the room the budget leaves under the cap.
    let mut shown = Vec::with_capacity(lines.len());
    let mut notes = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let line_no = offset as usize + i;
        let (text, note) = cut_line(line, line_no, if i == 0 { character as usize } else { 1 });
        shown.push(text);
        notes.push(note);
    }
    let (kept, truncated) = fit_lines(shown, LINES_BUDGET_CHARS, PER_LINE);
    // `fit_lines` keeps a prefix, so `notes` still lines up index for index.
    let numbered = kept
        .iter()
        .enumerate()
        .map(|(i, line)| match notes.get(i) {
            Some(Some(note)) => format!("{:>6}\t{line}{note}", offset as usize + i),
            _ => format!("{:>6}\t{line}", offset as usize + i),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut response = serde_json::json!({
        "content": numbered,
        "total_lines": contents.total_lines,
        "hash": contents.hash,
    });
    let next = offset as usize + kept.len();
    if next <= contents.total_lines {
        response["next_offset"] = Value::from(next);
    }
    if truncated {
        response["truncated"] = Value::Bool(true);
    }
    // Only a read that starts mid-line says so: the text it got isn't the
    // line's start. Nothing is added inside `content`, so text copied out of
    // a read into `edit_file` stays real file text.
    if character > 1 {
        response["character"] = Value::from(character);
    }
    Ok(response)
}

/// The most of one line `read_file` shows (SME-76's `MAX_LINE_CHARS`), and
/// the note a cut one wears.
///
/// `line` is what the read shows of it — whole, or from character `start`
/// onward when the read starts mid-line — and `line_no` its 1-based line
/// number. Characters are counted with `chars()`, never by byte index, so a
/// cut can't split one. Returns the text to show (at most `MAX_LINE_CHARS`
/// characters) and, for a cut line, its note: which characters of which line
/// it shows, how long the line is, and the `offset` and `character` to read
/// on with. The note goes on the end, so nothing a model might copy into
/// `edit_file`'s `old_string` appears at the front.
fn cut_line(line: &str, line_no: usize, start: usize) -> (String, Option<String>) {
    let length = line.chars().count();
    if length <= MAX_LINE_CHARS {
        return (line.to_string(), None);
    }
    let shown: String = line.chars().take(MAX_LINE_CHARS).collect();
    let (first, last) = (start, start + MAX_LINE_CHARS - 1);
    let note = format!(
        "… [line {line_no} cut: characters {first}–{last} of {} shown; \
         read on with offset {line_no}, character {}]",
        start + length - 1,
        last + 1
    );
    (shown, Some(note))
}

/// Overwriting an existing path requires it to have already been
/// `read_file`'d (or written/edited) earlier in this conversation —
/// `find_prior_file_hash` returning `None` means either a brand-new
/// path (no prior operation exists to have found) or a path this
/// conversation genuinely hasn't touched yet; either way `write_file`
/// treats it as a new file and skips the check (nothing to compare
/// against), matching SME-11's "creating a brand-new file... doesn't
/// need this." `edit_file`, below, is stricter — it always needs
/// `old_string` to have come from somewhere, so a missing prior hash
/// there is a hard refusal instead.
async fn write_file_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let content = required_str(input, "content")?;

    let messages = db::list_messages(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let expected_hash = find_prior_file_hash(&messages, &path);

    let hash = sandbox::write_file(pool, conversation_id, &path, &content, expected_hash)
        .await
        .map_err(|e| e.to_string())?;
    Ok(edit_result(pool, conversation_id, &path, hash).await)
}

pub(super) async fn edit_file_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let old_string = required_str(input, "old_string")?;
    let new_string = required_str(input, "new_string")?;
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let expected_line = input
        .get("expected_line")
        .and_then(Value::as_u64)
        .map(|v| v as u32);

    if replace_all && expected_line.is_some() {
        return Err("replace_all and expected_line are mutually exclusive — replace_all means every occurrence, expected_line means exactly one".to_string());
    }

    let messages = db::list_messages(pool, conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let expected_hash = find_prior_file_hash(&messages, &path).ok_or_else(|| {
        format!("{path} hasn't been read in this conversation yet — call read_file first")
    })?;

    let hash = sandbox::edit_file(
        pool,
        conversation_id,
        &path,
        &old_string,
        &new_string,
        replace_all,
        expected_hash,
        expected_line,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(edit_result(pool, conversation_id, &path, hash).await)
}

/// An edit's result: its hash, and the file's diagnostics when a
/// language server that takes it is running (SME-35).
async fn edit_result(pool: &PgPool, conversation_id: i64, path: &str, hash: String) -> String {
    let diagnostics = crate::lsp::manager::diagnostics_after_edit(pool, conversation_id, path).await;
    edit_result_json(hash, diagnostics)
}

/// `edit_result`'s JSON. The diagnostics are fitted to
/// `LINES_BUDGET_CHARS` (`fit_lines`) so the result stays under
/// `execute`'s cap: they serialize before `hash`, and a cut there lost
/// the new hash and left invalid JSON (SME-76's review).
pub(super) fn edit_result_json(hash: String, diagnostics: Option<String>) -> String {
    let mut result = serde_json::json!({"hash": hash});
    if let Some(diagnostics) = diagnostics {
        let lines: Vec<String> = diagnostics.lines().map(str::to_string).collect();
        let total = lines.len();
        // the escaped newline joining each line
        const PER_LINE: usize = 2;
        let (kept, truncated) = fit_lines(lines, LINES_BUDGET_CHARS, PER_LINE);
        let mut shown = kept.join("\n");
        if truncated {
            shown.push_str(&format!("\n[diagnostics cut: {} of {total} lines shown]", kept.len()));
        }
        result["diagnostics"] = Value::String(shown);
    }
    result.to_string()
}

async fn list_directory_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let entries = sandbox::list_directory(pool, conversation_id, &path)
        .await
        .map_err(|e| e.to_string())?;
    let payload: Vec<_> = entries
        .iter()
        .map(|e| serde_json::json!({"name": e.name, "type": if e.is_dir { "dir" } else { "file" }, "size": e.size}))
        .collect();
    Ok(serde_json::json!({"entries": payload}).to_string())
}

const DEFAULT_GLOB_LIMIT: u32 = 50;

const MAX_GLOB_LIMIT: u32 = 200;

const DEFAULT_GREP_LIMIT: u32 = 20;

const MAX_GREP_LIMIT: u32 = 100;

/// Shared by `glob_tool`/`grep_tool` — same 1-indexed-offset,
/// clamped-limit shape `read_file_tool` already uses.
fn pagination_params(input: &Value, default_limit: u32, max_limit: u32) -> (u32, u32) {
    let offset = input
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as u32;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default_limit as u64)
        .clamp(1, max_limit as u64) as u32;
    (offset, limit)
}

async fn glob_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let pattern = required_str(input, "pattern")?;
    let (offset, limit) = pagination_params(input, DEFAULT_GLOB_LIMIT, MAX_GLOB_LIMIT);
    let result = sandbox::glob(pool, conversation_id, &path, &pattern, offset, limit)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "paths": result.paths,
        "total": result.total,
        "scan_capped": result.scan_capped,
    })
    .to_string())
}

async fn grep_tool(
    pool: &PgPool,
    conversation_id: i64,
    input: &Value,
) -> Result<String, String> {
    let path = required_str(input, "path")?;
    let pattern = required_str(input, "pattern")?;
    let glob = input
        .get("glob")
        .and_then(Value::as_str)
        .map(str::to_string);
    let case_insensitive = input
        .get("case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (offset, limit) = pagination_params(input, DEFAULT_GREP_LIMIT, MAX_GREP_LIMIT);
    let result = sandbox::grep(
        pool,
        conversation_id,
        &path,
        &pattern,
        glob,
        case_insensitive,
        offset,
        limit,
    )
    .await
    .map_err(|e| e.to_string())?;
    let matches: Vec<_> = result
        .matches
        .iter()
        .map(|m| serde_json::json!({"path": m.path, "line": m.line, "text": m.text}))
        .collect();
    let skipped: Vec<_> = result
        .skipped
        .iter()
        .map(|s| serde_json::json!({"path": s.path, "reason": s.reason}))
        .collect();
    Ok(serde_json::json!({
        "matches": matches,
        "total": result.total,
        "scan_capped": result.scan_capped,
        "skipped": skipped,
    })
    .to_string())
}

#[cfg(test)]
mod tests;

