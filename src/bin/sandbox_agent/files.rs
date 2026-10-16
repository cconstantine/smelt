use super::*;

/// SHA-256 hex digest of a file's full content — what `read_file` returns
/// alongside its (possibly paginated) slice, and what `edit_file`/
/// `write_file` compare an `expected_hash` against before writing. See
/// SME-11's "Change detection, not just 'was it
/// read.'"
pub(super) fn hash_content(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Every non-overlapping byte offset where `needle` occurs in `haystack`,
/// in order — the basis for `edit_file`'s "exactly one match" / `replace_all`
/// / `expected_line` logic (see `apply_edit`). Non-overlapping so a needle
/// like "aa" against "aaa" reports one match, not two — matches ordinary
/// substring-replace semantics (`str::replace`'s own behavior), not every
/// possible overlapping window.
pub(super) fn find_matches(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut offsets = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = haystack[search_from..].find(needle) {
        let offset = search_from + pos;
        offsets.push(offset);
        search_from = offset + needle.len();
    }
    offsets
}

/// 1-indexed line number containing a byte offset — how a `find_matches`
/// result is checked against `edit_file`'s `expected_line`, and how
/// `read_file`'s line-numbered output is produced. Counts newlines strictly
/// before `offset`.
pub(super) fn byte_offset_to_line(text: &str, offset: usize) -> u32 {
    1 + text.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// `edit_file`'s core replace logic — see
/// SME-11's "What"/"How" on `edit_file`.
#[derive(Debug, PartialEq)]
pub(super) enum EditError {
    /// `old_string` doesn't occur in the file at all.
    NotFound,
    /// Multiple matches and neither `replace_all` nor `expected_line` was
    /// given to disambiguate which one was meant.
    Ambiguous { count: usize },
    /// `expected_line` was given, but no match starts on that line (there
    /// may still be matches elsewhere in the file).
    NoMatchAtLine { line: u32 },
}

pub(super) fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_line: Option<u32>,
) -> Result<String, EditError> {
    let matches = find_matches(content, old_string);
    if matches.is_empty() {
        return Err(EditError::NotFound);
    }

    if replace_all {
        return Ok(content.replace(old_string, new_string));
    }

    let offset = if let Some(line) = expected_line {
        *matches
            .iter()
            .find(|&&offset| byte_offset_to_line(content, offset) == line)
            .ok_or(EditError::NoMatchAtLine { line })?
    } else if matches.len() > 1 {
        return Err(EditError::Ambiguous {
            count: matches.len(),
        });
    } else {
        matches[0]
    };

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..offset]);
    result.push_str(new_string);
    result.push_str(&content[offset + old_string.len()..]);
    Ok(result)
}

/// `read_file`'s pagination — `offset` is a 1-indexed starting line
/// (already defaulted/clamped by the caller, same convention as
/// `read_terminal_output`'s offset/limit). Returns the requested slice of
/// lines plus the file's *total* line count, so a partial read is
/// distinguishable from the whole file.
pub(super) fn paginate_lines(content: &str, offset: u32, limit: u32) -> (Vec<String>, usize) {
    let all_lines: Vec<&str> = content.lines().collect();
    let total = all_lines.len();
    let skip = offset.saturating_sub(1) as usize;
    let slice = all_lines
        .into_iter()
        .skip(skip)
        .take(limit as usize)
        .map(str::to_string)
        .collect();
    (slice, total)
}

/// Compiles `pattern` into a matcher shared by `glob`'s own `pattern` and
/// `grep`'s optional `glob` filter — see
/// SME-19's "Decisions from review."
/// `literal_separator(true)` is deliberate, not `globset`'s own default:
/// checked against the real crate source (not assumed), a bare
/// `globset::Glob` lets `*` cross a `/` (so plain `*.rs`, with no `**`,
/// would already match `src/main.rs`) — indistinguishable from `**/*.rs`
/// and defeating the whole point of documenting `**` as the recursive
/// marker in this tool's own schema. `literal_separator(true)` restores
/// the shell/ripgrep-familiar meaning: a bare `*` matches within one path
/// segment, only an explicit `**/` recurses.
pub(super) fn compile_glob_pattern(pattern: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))
}

/// Compiles `pattern` for `grep` — shared error-formatting wrapper around
/// `regex::RegexBuilder`, same reason as `compile_glob_pattern`.
pub(super) fn compile_grep_pattern(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| format!("invalid regex pattern {pattern:?}: {e}"))
}

/// Scans `content` line by line for `pattern`, returning up to `budget`
/// `(1-indexed line number, line text)` matches and whether `budget` was
/// exhausted before every line was checked (distinct from the file simply
/// having no more matches). One file's worth of work — the walk loop that
/// calls this per file (only exercised by the real-cluster integration
/// test, not unit-tested here — see SME-19)
/// owns combining this across every file into the overall `scan_capped`.
pub(super) fn grep_content(
    content: &str,
    pattern: &regex::Regex,
    budget: usize,
) -> (Vec<(u32, String)>, bool) {
    let mut matches = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if matches.len() >= budget {
            return (matches, true);
        }
        if pattern.is_match(line) {
            matches.push((i as u32 + 1, line.to_string()));
        }
    }
    (matches, false)
}

/// Same offset/limit slicing as `paginate_lines`, generalized to an
/// already-materialized slice rather than `&str` split into lines — what
/// `glob`/`grep` page their (pre-capped, see `MAX_GLOB_SCAN`/
/// `MAX_GREP_SCAN`) match lists with. `total` is `items.len()`, not the
/// returned page's length.
pub(super) fn paginate_slice<T: Clone>(items: &[T], offset: u32, limit: u32) -> (Vec<T>, usize) {
    let total = items.len();
    let skip = offset.saturating_sub(1) as usize;
    let page = items
        .iter()
        .skip(skip)
        .take(limit as usize)
        .cloned()
        .collect();
    (page, total)
}

/// Alphabetical by name, case-sensitive (Rust's default `str` ordering —
/// uppercase sorts before lowercase) — `list_directory` doesn't group
/// directories first, just a plain sort, see SME-11's "What."
pub(super) fn sort_entries(mut entries: Vec<DirEntry>) -> Vec<DirEntry> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// What `edit_file` reports back to the model for each `EditError` variant
/// — the structured error SME-11's "Which files" bullet calls for (hash
/// mismatch / not found / ambiguous with match count / no match at the
/// given line).
pub(super) fn edit_error_message(err: EditError) -> String {
    match err {
        EditError::NotFound => "old_string not found in file".to_string(),
        EditError::Ambiguous { count } => {
            format!(
                "old_string matches {count} times; add more surrounding context, set replace_all, or set expected_line to target one occurrence"
            )
        }
        EditError::NoMatchAtLine { line } => {
            format!("old_string does not match at line {line}")
        }
    }
}

/// Shared across `read_file`/`edit_file`/`write_file` — an unbounded file
/// becomes an unbounded tool-result message persisted into Postgres and
/// re-sent on every subsequent turn, see
/// SME-11's "Size bound."
const MAX_FILE_SIZE_BYTES: u64 = 256 * 1024;
/// `list_directory`'s analogous bound, as an entry count rather than bytes
/// — same reasoning, see SME-11's "Size bound."
const MAX_DIR_ENTRIES: usize = 1000;
/// `grep` skips (never errors out entirely on, see `SkippedFile`) a
/// file bigger than this — bounds worst-case per-file scan time. Distinct
/// from `MAX_FILE_SIZE_BYTES` above, which caps *returned* content
/// (`read_file`'s concern); grep's returned payload is already bounded by
/// its match count, not the size of what it searched through.
const MAX_GREP_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;
/// Hard ceiling on matches `glob`/`grep` accumulate across the whole walk,
/// independent of the page (`offset`/`limit`) actually requested — bounds
/// worst-case walk cost on a huge tree or a pathological pattern.
/// `scan_capped` in the response is true only when *this* was hit, not
/// just the requested page — see
/// SME-19's "Decisions from review."
const MAX_GLOB_SCAN: usize = 2000;
const MAX_GREP_SCAN: usize = 1000;

pub(super) fn reply_error(message: &str) -> Reply {
    Reply::Error { message: message.to_string() }
}

pub(super) async fn handle_read_file(
    path: String,
    offset: u32,
    limit: u32,
) -> Reply {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return reply_error(&format!("failed to read {path}: {e}"));
        }
    };
    if bytes.len() as u64 > MAX_FILE_SIZE_BYTES {
        return reply_error(&format!("{path} exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit"));
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return reply_error(&format!("{path} is not valid UTF-8"));
        }
    };
    let hash = hash_content(content.as_bytes());
    let (lines, total_lines) = paginate_lines(&content, offset, limit);
    Reply::FileRead(FileContents { lines, total_lines, hash })
}

pub(super) async fn handle_write_file(
    path: String,
    content: String,
    expected_hash: Option<String>,
) -> Reply {
    if content.len() as u64 > MAX_FILE_SIZE_BYTES {
        return reply_error(&format!("content exceeds the {MAX_FILE_SIZE_BYTES}-byte size limit"));
    }

    if let Some(expected) = &expected_hash {
        match tokio::fs::read(&path).await {
            Ok(current) => {
                let current_hash = hash_content(&current);
                if &current_hash != expected {
                    return reply_error(&format!("{path} has changed since it was last read (expected hash {expected}, found {current_hash}); read_file again before writing"));
                }
            }
            Err(e) => {
                return reply_error(&format!("failed to read {path} for hash check: {e}"));
            }
        }
    }

    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return reply_error(&format!("failed to create parent directories for {path}: {e}"));
            }
        }
    }

    if let Err(e) = tokio::fs::write(&path, content.as_bytes()).await {
        return reply_error(&format!("failed to write {path}: {e}"));
    }
    let hash = hash_content(content.as_bytes());
    Reply::FileWritten { hash }
}

pub(super) async fn handle_edit_file(
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
    expected_hash: String,
    expected_line: Option<u32>,
) -> Reply {
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return reply_error(&format!("failed to read {path}: {e}"));
        }
    };
    let current_hash = hash_content(&bytes);
    if current_hash != expected_hash {
        return reply_error(&format!("{path} has changed since it was last read (expected hash {expected_hash}, found {current_hash}); read_file again before editing"));
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(_) => {
            return reply_error(&format!("{path} is not valid UTF-8"));
        }
    };

    let new_content = match apply_edit(
        &content,
        &old_string,
        &new_string,
        replace_all,
        expected_line,
    ) {
        Ok(new_content) => new_content,
        Err(edit_err) => {
            return reply_error(&edit_error_message(edit_err));
        }
    };

    if let Err(e) = tokio::fs::write(&path, new_content.as_bytes()).await {
        return reply_error(&format!("failed to write {path}: {e}"));
    }
    let hash = hash_content(new_content.as_bytes());
    Reply::FileEdited { hash }
}

pub(super) async fn handle_list_directory(path: String) -> Reply {
    let mut read_dir = match tokio::fs::read_dir(&path).await {
        Ok(read_dir) => read_dir,
        Err(e) => {
            return reply_error(&format!("failed to read directory {path}: {e}"));
        }
    };

    let mut entries = Vec::new();
    loop {
        match read_dir.next_entry().await {
            Ok(Some(entry)) => {
                if entries.len() >= MAX_DIR_ENTRIES {
                    return reply_error(&format!("{path} has more than {MAX_DIR_ENTRIES} entries"));
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                let size = if is_dir {
                    None
                } else {
                    entry.metadata().await.ok().map(|m| m.len())
                };
                entries.push(DirEntry { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => {
                return reply_error(&format!("failed to read directory {path}: {e}"));
            }
        }
    }

    let entries = sort_entries(entries);
    Reply::DirectoryListed { entries }
}

/// Finds files under `path` whose path relative to `path` matches
/// `pattern` — see SME-19. Real filesystem
/// walk (via `ignore::WalkBuilder`, `.gitignore`-aware even outside a real
/// git checkout), not unit-tested directly — only `compile_glob_pattern`
/// and `paginate_slice`, its pure pieces, are; this glue is exercised by
/// the real-cluster integration test instead, same as
/// `handle_list_directory` above already is.
pub(super) async fn handle_glob(
    path: String,
    pattern: String,
    offset: u32,
    limit: u32,
) -> Reply {
    let matcher = match compile_glob_pattern(&pattern) {
        Ok(matcher) => matcher,
        Err(e) => {
            return reply_error(&e);
        }
    };
    let walked = tokio::task::spawn_blocking(move || walk_glob(&path, &matcher)).await;
    let (all_paths, scan_capped) = match walked {
        Ok(result) => result,
        Err(_) => {
            return reply_error("internal error walking directory");
        }
    };
    let (paths, total) = paginate_slice(&all_paths, offset, limit);
    Reply::GlobMatched(GlobResult { paths, total, scan_capped })
}

/// Synchronous (`ignore`'s walker is not async) — must run inside
/// `spawn_blocking`, never called directly from async code. Stops once
/// `MAX_GLOB_SCAN` matches are found, reporting that as `scan_capped`
/// rather than silently truncating.
fn walk_glob(root: &str, matcher: &globset::GlobMatcher) -> (Vec<String>, bool) {
    let root_path = std::path::Path::new(root);
    let mut paths = Vec::new();
    let mut scan_capped = false;
    let walker = ignore::WalkBuilder::new(root_path)
        .git_ignore(true)
        .require_git(false)
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            continue;
        }
        let relative = entry.path().strip_prefix(root_path).unwrap_or(entry.path());
        if !matcher.is_match(relative) {
            continue;
        }
        if paths.len() >= MAX_GLOB_SCAN {
            scan_capped = true;
            break;
        }
        paths.push(entry.path().to_string_lossy().into_owned());
    }
    (paths, scan_capped)
}

/// Searches file contents under `path` for `pattern`, optionally narrowed
/// to files matching `glob` first — same walk/testing split as
/// `handle_glob` above.
pub(super) async fn handle_grep(
    path: String,
    pattern: String,
    glob: Option<String>,
    case_insensitive: bool,
    offset: u32,
    limit: u32,
) -> Reply {
    let pattern = match compile_grep_pattern(&pattern, case_insensitive) {
        Ok(pattern) => pattern,
        Err(e) => {
            return reply_error(&e);
        }
    };
    let glob_matcher = match glob.as_deref().map(compile_glob_pattern) {
        Some(Ok(matcher)) => Some(matcher),
        Some(Err(e)) => {
            return reply_error(&e);
        }
        None => None,
    };
    let walked =
        tokio::task::spawn_blocking(move || walk_grep(&path, &pattern, glob_matcher.as_ref()))
            .await;
    let (all_matches, scan_capped, skipped) = match walked {
        Ok(result) => result,
        Err(_) => {
            return reply_error("internal error walking directory");
        }
    };
    let (matches, total) = paginate_slice(&all_matches, offset, limit);
    Reply::GrepMatched(GrepResult { matches, total, scan_capped, skipped })
}

/// Synchronous, same `spawn_blocking`-only rule as `walk_glob`. Every file
/// it excludes (oversized, or not valid UTF-8) lands in the returned
/// `skipped` list — never silently dropped, per SME-19's "Decisions
/// from review." `scan_capped` combines the walk's own cap with
/// `grep_content`'s per-file budget exhaustion — either one means there
/// may be more matches than `total` reports.
fn walk_grep(
    root: &str,
    pattern: &regex::Regex,
    glob_matcher: Option<&globset::GlobMatcher>,
) -> (Vec<GrepMatch>, bool, Vec<SkippedFile>) {
    let root_path = std::path::Path::new(root);
    let mut matches = Vec::new();
    let mut skipped = Vec::new();
    let mut scan_capped = false;
    let walker = ignore::WalkBuilder::new(root_path)
        .git_ignore(true)
        .require_git(false)
        .build();
    for entry in walker {
        if matches.len() >= MAX_GREP_SCAN {
            scan_capped = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            continue;
        }
        let relative = entry.path().strip_prefix(root_path).unwrap_or(entry.path());
        if let Some(matcher) = glob_matcher {
            if !matcher.is_match(relative) {
                continue;
            }
        }
        let display_path = entry.path().to_string_lossy().into_owned();
        let size = match entry.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        if size > MAX_GREP_FILE_SIZE_BYTES {
            skipped.push(SkippedFile {
                path: display_path,
                reason: format!("too large ({size} bytes > {MAX_GREP_FILE_SIZE_BYTES} byte limit)"),
            });
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(_) => {
                skipped.push(SkippedFile {
                    path: display_path,
                    reason: "binary".to_string(),
                });
                continue;
            }
        };
        let budget = MAX_GREP_SCAN.saturating_sub(matches.len());
        let (file_matches, file_capped) = grep_content(&content, pattern, budget);
        if file_capped {
            scan_capped = true;
        }
        for (line, text) in file_matches {
            matches.push(GrepMatch {
                path: display_path.clone(),
                line,
                text,
            });
        }
    }
    (matches, scan_capped, skipped)
}
