//! The `lsp` tool's operations (SME-35), and turning their LSP answers into
//! text the model reads: `path:line:col` with the source line, not JSON.
//! Positions are 1-based lines and characters for the model, and LSP's
//! 0-based lines and UTF-16 columns for the server.

use serde_json::Value;

use crate::models::LanguageServerConfig;

/// An `lsp` tool operation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operation {
    Definition,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    Diagnostics,
    Implementation,
    IncomingCalls,
    OutgoingCalls,
    Rename,
}

impl Operation {
    pub const ALL: [Operation; 10] = [
        Operation::Definition,
        Operation::References,
        Operation::Hover,
        Operation::DocumentSymbols,
        Operation::WorkspaceSymbols,
        Operation::Diagnostics,
        Operation::Implementation,
        Operation::IncomingCalls,
        Operation::OutgoingCalls,
        Operation::Rename,
    ];

    pub fn parse(name: &str) -> Result<Operation, String> {
        Operation::ALL.into_iter().find(|op| op.name() == name).ok_or_else(|| {
            let names: Vec<&str> = Operation::ALL.iter().map(|op| op.name()).collect();
            format!("Unknown operation {name:?}: it's one of {}.", names.join(", "))
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Operation::Definition => "definition",
            Operation::References => "references",
            Operation::Hover => "hover",
            Operation::DocumentSymbols => "document_symbols",
            Operation::WorkspaceSymbols => "workspace_symbols",
            Operation::Diagnostics => "diagnostics",
            Operation::Implementation => "implementation",
            Operation::IncomingCalls => "incoming_calls",
            Operation::OutgoingCalls => "outgoing_calls",
            Operation::Rename => "rename",
        }
    }

    /// The server capability it needs, from `initialize`'s answer.
    pub fn capability(self) -> Option<&'static str> {
        match self {
            Operation::Definition => Some("definitionProvider"),
            Operation::References => Some("referencesProvider"),
            Operation::Hover => Some("hoverProvider"),
            Operation::DocumentSymbols => Some("documentSymbolProvider"),
            Operation::WorkspaceSymbols => Some("workspaceSymbolProvider"),
            Operation::Diagnostics => None,
            Operation::Implementation => Some("implementationProvider"),
            Operation::IncomingCalls | Operation::OutgoingCalls => Some("callHierarchyProvider"),
            Operation::Rename => Some("renameProvider"),
        }
    }

    /// Whether it works on a position in a file (the others take a file,
    /// or a query).
    pub fn needs_position(self) -> bool {
        !matches!(self, Operation::DocumentSymbols | Operation::WorkspaceSymbols | Operation::Diagnostics)
    }
}

/// How many results a list shows before "and N more".
pub const MAX_RESULTS: usize = 50;

/// The UTF-16 column (0-based) of 1-based character `character` in `line`.
pub fn to_utf16(line: &str, character: u32) -> u32 {
    line.chars().take(character.saturating_sub(1) as usize).map(|c| c.len_utf16() as u32).sum()
}

/// The 1-based character of UTF-16 column `column` in `line`.
pub fn from_utf16(line: &str, column: u32) -> u32 {
    let mut units = 0;
    let mut characters = 0;
    for c in line.chars() {
        if units >= column {
            break;
        }
        units += c.len_utf16() as u32;
        characters += 1;
    }
    characters + 1
}

/// A location's `(path, 0-based line, UTF-16 column)`.
fn location_parts(location: &Value) -> Option<(String, u32, u32)> {
    let uri = location.get("uri").or_else(|| location.get("targetUri"))?.as_str()?;
    let range = location
        .get("targetSelectionRange")
        .or_else(|| location.get("range"))
        .or_else(|| location.get("targetRange"))?;
    let start = range.get("start")?;
    Some((
        crate::lsp::session::uri_path(uri)?,
        start.get("line")?.as_u64()? as u32,
        start.get("character")?.as_u64()? as u32,
    ))
}

/// `path:line:col` (1-based), with the source line when it can be read.
fn location_line(path: &str, line: u32, column: u32, source_line: &mut dyn FnMut(&str, u32) -> Option<String>) -> String {
    match source_line(path, line) {
        Some(text) => format!("{path}:{}:{}  {}", line + 1, from_utf16(&text, column), text.trim()),
        None => format!("{path}:{}:{}", line + 1, column + 1),
    }
}

/// `lines`, cut at `MAX_RESULTS` with a count of the rest.
fn capped(lines: Vec<String>) -> String {
    let total = lines.len();
    let mut shown: Vec<String> = lines.into_iter().take(MAX_RESULTS).collect();
    if total > MAX_RESULTS {
        shown.push(format!("…and {} more", total - MAX_RESULTS));
    }
    shown.join("\n")
}

/// Locations (`Location`, `Location[]` or `LocationLink[]`) as lines of
/// `path:line:col  source`. `source_line(path, line)` gives a file's line
/// (0-based), when it can be read.
pub fn format_locations(value: &Value, source_line: &mut dyn FnMut(&str, u32) -> Option<String>) -> String {
    let locations: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    let lines: Vec<String> = locations
        .into_iter()
        .filter_map(location_parts)
        .map(|(path, line, column)| location_line(&path, line, column, source_line))
        .collect();
    if lines.is_empty() {
        return "No results.".to_string();
    }
    capped(lines)
}

/// A hover's contents as plain text.
pub fn format_hover(value: &Value) -> String {
    fn part(value: &Value) -> Option<String> {
        match value {
            Value::String(text) => Some(text.clone()),
            Value::Object(object) => object.get("value").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
    }
    let text = match value.get("contents") {
        Some(Value::Array(parts)) => parts.iter().filter_map(part).collect::<Vec<_>>().join("\n\n"),
        Some(contents) => part(contents).unwrap_or_default(),
        None => String::new(),
    };
    let text = text.trim();
    if text.is_empty() {
        return "No hover information here.".to_string();
    }
    crate::fetch_guard::truncate(text.to_string(), 4000).0
}

/// LSP's `SymbolKind`, by number.
fn symbol_kind(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "symbol", "file", "module", "namespace", "package", "class", "method", "property", "field", "constructor",
        "enum", "interface", "function", "variable", "constant", "string", "number", "boolean", "array", "object",
        "key", "null", "enum member", "struct", "event", "operator",
    ];
    KINDS.get(kind as usize).copied().unwrap_or("symbol")
}

/// `DocumentSymbol[]` (nested, from `document`) or
/// `SymbolInformation[]`/`WorkspaceSymbol[]` as one symbol a line, with
/// 1-based characters where `source_line` has the line.
pub fn format_symbols(value: &Value, document: Option<&str>, source_line: &mut dyn FnMut(&str, u32) -> Option<String>) -> String {
    // A 1-based character for a UTF-16 column, where the line can be read.
    let mut character = |path: &str, line: u32, column: u32| match source_line(path, line) {
        Some(text) => from_utf16(&text, column),
        None => column + 1,
    };
    fn nested(symbols: &[Value], depth: usize, out: &mut Vec<String>, character: &mut dyn FnMut(u32, u32) -> u32) {
        for symbol in symbols {
            let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
            let start = symbol.get("selectionRange").or_else(|| symbol.get("range")).and_then(|r| r.get("start"));
            let line = start.and_then(|s| s.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32;
            let column = start.and_then(|s| s.get("character")).and_then(Value::as_u64).unwrap_or(0) as u32;
            out.push(format!("{}{kind} {name}  {}:{}", "  ".repeat(depth), line + 1, character(line, column)));
            if let Some(children) = symbol.get("children").and_then(Value::as_array) {
                nested(children, depth + 1, out, character);
            }
        }
    }
    let Some(symbols) = value.as_array().filter(|s| !s.is_empty()) else {
        return "No symbols.".to_string();
    };
    let mut lines = Vec::new();
    if symbols[0].get("location").is_some() {
        for symbol in symbols {
            let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
            let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
            let place = symbol
                .get("location")
                .and_then(location_parts)
                .map(|(path, line, column)| format!("{path}:{}:{}", line + 1, character(&path, line, column)))
                .unwrap_or_default();
            lines.push(format!("{kind} {name}  {place}"));
        }
    } else {
        let document = document.unwrap_or_default();
        nested(symbols, 0, &mut lines, &mut |line, column| character(document, line, column));
    }
    capped(lines)
}

/// Diagnostics as `line:col severity: message`, errors and warnings only
/// when `serious_only`.
pub fn format_diagnostics(items: &[Value], source_line: &mut dyn FnMut(u32) -> Option<String>, serious_only: bool) -> String {
    let mut lines = Vec::new();
    for item in items {
        // Absent means the client decides: counted as an error.
        let severity = item.get("severity").and_then(Value::as_u64).unwrap_or(1);
        if serious_only && severity > 2 {
            continue;
        }
        let severity = match severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        };
        let start = item.get("range").and_then(|r| r.get("start"));
        let line = start.and_then(|s| s.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32;
        let column = start.and_then(|s| s.get("character")).and_then(Value::as_u64).unwrap_or(0) as u32;
        let code = match item.get("code") {
            Some(Value::String(code)) => format!("[{code}]"),
            Some(Value::Number(code)) => format!("[{code}]"),
            _ => String::new(),
        };
        let message = item.get("message").and_then(Value::as_str).unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
        let text = source_line(line);
        let column = text.as_deref().map(|t| from_utf16(t, column)).unwrap_or(column + 1);
        let mut entry = format!("{}:{column} {severity}{code}: {message}", line + 1);
        if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
            entry.push_str(&format!("  ({})", text.trim()));
        }
        lines.push(entry);
    }
    capped(lines)
}

/// The server to use for `path`: a running one (`running` names them)
/// among the enabled configs that take its extension. The error says what
/// the model can do instead.
pub fn choose_server<'a>(
    path: &str,
    configs: &'a [LanguageServerConfig],
    running: &[String],
) -> Result<&'a LanguageServerConfig, String> {
    if !crate::lsp::session::in_workspace(path) {
        return Err(format!(
            "{path} isn't under /workspace: language servers only see /workspace. Work there to use them."
        ));
    }
    let extension = path.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('/')).unwrap_or_default();
    let takers: Vec<&LanguageServerConfig> =
        configs.iter().filter(|c| c.enabled && c.file_types.contains_key(extension)).collect();
    if let Some(running) = takers.iter().find(|c| running.contains(&c.name)) {
        return Ok(running);
    }
    if takers.is_empty() {
        let enabled: Vec<&str> = configs.iter().filter(|c| c.enabled).map(|c| c.name.as_str()).collect();
        let listed = if enabled.is_empty() { "none are".to_string() } else { format!("configured: {}", enabled.join(", ")) };
        return Err(format!(
            "No language server takes .{extension} files ({listed}). The user adds them on the Language servers page."
        ));
    }
    let names: Vec<&str> = takers.iter().map(|c| c.name.as_str()).collect();
    Err(format!(
        "No language server for .{extension} files is running. Start one with start_language_server: {}.",
        names.join(" or ")
    ))
}

/// A `WorkspaceEdit` as files and their text edits: from `changes` or
/// `documentChanges`. Creating, renaming or deleting files is refused.
pub fn workspace_edit_files(edit: &Value) -> Result<Vec<(String, Vec<Value>)>, String> {
    let mut files: Vec<(String, Vec<Value>)> = Vec::new();
    let mut add = |uri: &str, edits: Vec<Value>| -> Result<(), String> {
        let path = crate::lsp::session::uri_path(uri).ok_or_else(|| format!("the server edits {uri}, not a file"))?;
        if !crate::lsp::session::in_workspace(&path) {
            return Err(format!("the server edits {path}, outside /workspace; nothing was changed"));
        }
        match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, existing)) => existing.extend(edits),
            None => files.push((path, edits)),
        }
        Ok(())
    };
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            if change.get("kind").is_some() {
                return Err("the rename would also create, rename or delete a file, which smelt doesn't do; nothing was changed".to_string());
            }
            let uri = change.get("textDocument").and_then(|d| d.get("uri")).and_then(Value::as_str).unwrap_or_default();
            add(uri, change.get("edits").and_then(Value::as_array).cloned().unwrap_or_default())?;
        }
    } else if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            add(uri, edits.as_array().cloned().unwrap_or_default())?;
        }
    }
    Ok(files)
}

/// `text` with LSP `edits` (0-based lines, UTF-16 columns) applied.
pub fn apply_text_edits(text: &str, edits: &[Value]) -> Result<String, String> {
    // Each line's starting byte offset.
    let mut line_starts = vec![0usize];
    line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let offset = |position: &Value| -> Result<usize, String> {
        let line = position.get("line").and_then(Value::as_u64).ok_or("an edit without a line")? as usize;
        let column = position.get("character").and_then(Value::as_u64).ok_or("an edit without a character")? as u32;
        let start = *line_starts.get(line).ok_or_else(|| format!("an edit at line {} is past the end of the file", line + 1))?;
        let next = line_starts.get(line + 1).copied().unwrap_or(text.len());
        // The line without its break: a character past its end means its end.
        let line_text = text[start..next].trim_end_matches('\n').trim_end_matches('\r');
        let end = start + line_text.len();
        // UTF-16 units to a byte offset in the line.
        let mut units = 0u32;
        for (index, c) in line_text.char_indices() {
            if units >= column {
                return Ok(start + index);
            }
            units += c.len_utf16() as u32;
        }
        Ok(end)
    };
    let mut spans = Vec::new();
    for edit in edits {
        let range = edit.get("range").ok_or("an edit without a range")?;
        let from = offset(range.get("start").ok_or("an edit without a start")?)?;
        let to = offset(range.get("end").ok_or("an edit without an end")?)?;
        let new_text = edit.get("newText").and_then(Value::as_str).unwrap_or_default();
        spans.push((from, to.max(from), new_text));
    }
    spans.sort_by_key(|(from, to, _)| (*from, *to));
    if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("the server's edits overlap; nothing was changed".to_string());
    }
    let mut result = text.to_string();
    for (from, to, new_text) in spans.into_iter().rev() {
        result.replace_range(from..to, new_text);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> Value {
        json!({"range": {"start": {"line": start.0, "character": start.1}, "end": {"line": end.0, "character": end.1}}, "newText": text})
    }

    /// A character past the end of a line means the end of that line (the
    /// LSP spec), not the start of the next: the line break stays.
    #[test]
    fn test_a_character_past_the_end_of_a_line_means_its_end() {
        let text = "let a = 1;\nlet b = 2;\r\nlast";
        assert_eq!(apply_text_edits(text, &[edit((0, 8), (0, 999), "5;")]), Ok("let a = 5;\nlet b = 2;\r\nlast".to_string()));
        assert_eq!(apply_text_edits(text, &[edit((1, 8), (1, 999), "7;")]), Ok("let a = 1;\nlet b = 7;\r\nlast".to_string()));
        assert_eq!(apply_text_edits(text, &[edit((2, 999), (2, 999), "!")]), Ok("let a = 1;\nlet b = 2;\r\nlast!".to_string()));
    }

    #[test]
    fn test_text_edits_apply_from_last_to_first_in_utf16() {
        let text = "fn helper() {}\nfn main() { helper(); \"😀\"; helper(); }\n";
        let edits = vec![
            edit((0, 3), (0, 9), "assist"),
            edit((1, 12), (1, 18), "assist"),
            // After the emoji (2 UTF-16 units): column 28 in UTF-16.
            edit((1, 28), (1, 34), "assist"),
        ];
        assert_eq!(
            apply_text_edits(text, &edits),
            Ok("fn assist() {}\nfn main() { assist(); \"😀\"; assist(); }\n".to_string())
        );
        let overlapping = vec![edit((0, 0), (0, 5), "a"), edit((0, 3), (0, 8), "b")];
        assert!(apply_text_edits(text, &overlapping).is_err());
        assert!(apply_text_edits(text, &[edit((9, 0), (9, 1), "x")]).is_err(), "past the end");
    }

    #[test]
    fn test_a_workspace_edit_reads_as_files_either_way() {
        let changes = json!({"changes": {"file:///workspace/a.rs": [edit((0, 0), (0, 1), "x")]}});
        let files = workspace_edit_files(&changes).expect("changes");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "/workspace/a.rs");
        let document_changes = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///workspace/b.rs", "version": 3}, "edits": [edit((0, 0), (0, 1), "y")]},
        ]});
        assert_eq!(workspace_edit_files(&document_changes).expect("document changes")[0].0, "/workspace/b.rs");
        let renames_a_file = json!({"documentChanges": [{"kind": "rename", "oldUri": "file:///workspace/a.rs", "newUri": "file:///workspace/c.rs"}]});
        assert!(workspace_edit_files(&renames_a_file).unwrap_err().contains("file"));
        let outside = json!({"changes": {"file:///etc/passwd": [edit((0, 0), (0, 1), "x")]}});
        assert!(workspace_edit_files(&outside).is_err(), "only /workspace is written");
    }

    fn lines(text: &'static str) -> impl FnMut(&str, u32) -> Option<String> {
        move |_path, line| text.lines().nth(line as usize).map(str::to_string)
    }

    #[test]
    fn test_operations_parse_and_name_their_capability() {
        assert_eq!(Operation::parse("definition"), Ok(Operation::Definition));
        assert_eq!(Operation::parse("incoming_calls"), Ok(Operation::IncomingCalls));
        assert!(Operation::parse("teleport").unwrap_err().contains("definition"), "the error lists what exists");
        assert_eq!(Operation::References.capability(), Some("referencesProvider"));
        assert_eq!(Operation::IncomingCalls.capability(), Some("callHierarchyProvider"));
        assert_eq!(Operation::Diagnostics.capability(), None, "published diagnostics need no capability");
        assert_eq!(Operation::WorkspaceSymbols.name(), "workspace_symbols");
    }

    #[test]
    fn test_columns_convert_between_characters_and_utf16() {
        let line = "let é = \"😀x\";";
        // 1-based character 11 is `x`, after an emoji (2 UTF-16 units).
        assert_eq!(to_utf16(line, 11), 11);
        assert_eq!(from_utf16(line, 11), 11);
        assert_eq!(to_utf16(line, 10), 9, "the emoji itself starts at unit 9");
        assert_eq!(to_utf16(line, 5), 4, "é is one unit");
        assert_eq!(from_utf16(line, 4), 5);
        assert_eq!(to_utf16("ab", 9), 2, "past the end: the end");
    }

    #[test]
    fn test_locations_read_as_paths_with_their_lines() {
        let one = json!({"uri": "file:///workspace/a.rs", "range": {"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 9}}});
        let text = "fn main() {\n   helper();\n}\n";
        assert_eq!(format_locations(&one, &mut lines(text)), "/workspace/a.rs:2:4  helper();");
        let link = json!([{"targetUri": "file:///workspace/b.rs", "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "targetSelectionRange": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9}}}]);
        assert_eq!(format_locations(&link, &mut lines("fn helper() {}")), "/workspace/b.rs:1:4  fn helper() {}");
        assert_eq!(format_locations(&json!(null), &mut lines("")), "No results.");
        let many: Vec<Value> = (0..60).map(|i| json!({"uri": "file:///workspace/a.rs", "range": {"start": {"line": i, "character": 0}, "end": {"line": i, "character": 1}}})).collect();
        let formatted = format_locations(&json!(many), &mut |_, _| None);
        assert!(formatted.ends_with("…and 10 more"), "{formatted}");
    }

    #[test]
    fn test_hovers_read_as_text() {
        assert_eq!(format_hover(&json!({"contents": {"kind": "markdown", "value": "```rust\nfn helper() -> u32\n```"}})), "```rust\nfn helper() -> u32\n```");
        assert_eq!(format_hover(&json!({"contents": [{"language": "python", "value": "def f()"}, "Docs."]})), "def f()\n\nDocs.");
        assert_eq!(format_hover(&json!(null)), "No hover information here.");
    }

    /// Symbol columns are characters, as the tool takes them, not UTF-16
    /// units: they differ after an emoji.
    #[test]
    fn test_symbol_columns_are_characters() {
        let line = "let s = \"\u{1F600}\"; fn after() {}";
        let character = line.chars().position(|c| c == 'a').unwrap() as u32 + 1;
        let units = to_utf16(line, character);
        assert_ne!(units + 1, character, "the emoji makes them differ");
        let range = json!({"start": {"line": 0, "character": units}, "end": {"line": 0, "character": units + 5}});
        let mut lines = |_: &str, n: u32| (n == 0).then(|| line.to_string());
        let nested = json!([{"name": "after", "kind": 12, "range": range, "selectionRange": range}]);
        assert_eq!(format_symbols(&nested, Some("/workspace/a.rs"), &mut lines), format!("function after  1:{character}"));
        let flat = json!([{"name": "after", "kind": 12, "location": {"uri": "file:///workspace/a.rs", "range": range}}]);
        assert_eq!(format_symbols(&flat, None, &mut lines), format!("function after  /workspace/a.rs:1:{character}"));
    }

    #[test]
    fn test_symbols_read_one_to_a_line_nested_or_flat() {
        let nested = json!([{"name": "Config", "kind": 23, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1}},
            "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}},
            "children": [{"name": "new", "kind": 6, "range": {"start": {"line": 2, "character": 4}, "end": {"line": 4, "character": 5}},
                "selectionRange": {"start": {"line": 2, "character": 11}, "end": {"line": 2, "character": 14}}}]}]);
        assert_eq!(format_symbols(&nested, Some("/workspace/a.rs"), &mut |_, _| None), "struct Config  1:8\n  method new  3:12");
        let flat = json!([{"name": "helper", "kind": 12, "location": {"uri": "file:///workspace/a.rs", "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 9}}}}]);
        assert_eq!(format_symbols(&flat, None, &mut |_, _| None), "function helper  /workspace/a.rs:1:4");
    }

    #[test]
    fn test_diagnostics_read_as_lines_with_severity() {
        let items = vec![
            json!({"range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 9}}, "severity": 1, "message": "mismatched types\nexpected u32", "source": "rustc", "code": "E0308"}),
            json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 3, "message": "consider this"}),
        ];
        let mut source = |line: u32| (line == 4).then(|| "    let x: u32 = \"no\";".to_string());
        assert_eq!(
            format_diagnostics(&items, &mut source, true),
            "5:9 error[E0308]: mismatched types expected u32  (let x: u32 = \"no\";)"
        );
        assert!(format_diagnostics(&items, &mut |_| None, false).contains("1:1 info: consider this"));
    }

    fn server(name: &str, extensions: &[&str], enabled: bool) -> LanguageServerConfig {
        LanguageServerConfig {
            name: name.to_string(),
            file_types: extensions.iter().map(|e| (e.to_string(), "x".to_string())).collect(),
            enabled,
            ..Default::default()
        }
    }

    #[test]
    fn test_a_file_goes_to_a_running_server_that_takes_it() {
        let configs = vec![server("pyright", &["py"], true), server("pylsp", &["py"], true), server("ra", &["rs"], false)];
        let running = vec!["pylsp".to_string()];
        assert_eq!(choose_server("/workspace/a.py", &configs, &running).map(|c| c.name.as_str()), Ok("pylsp"));

        let not_started = choose_server("/workspace/a.py", &configs, &[]).unwrap_err();
        assert!(not_started.contains("start_language_server") && not_started.contains("pyright"), "{not_started}");

        let disabled = choose_server("/workspace/a.rs", &configs, &[]).unwrap_err();
        assert!(disabled.contains(".rs") && disabled.contains("pyright"), "names what exists: {disabled}");

        let outside = choose_server("/home/sandbox/a.py", &configs, &running).unwrap_err();
        assert!(outside.contains("/workspace"), "{outside}");
    }
}
