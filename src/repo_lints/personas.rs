//! The repo's own persona files (SME-247): every `.agents/personas/<name>.md`
//! parses through the same parser the server runs, the smelt-tool patterns in
//! its `smelt.tools` name tools the server registers, and the persona table
//! in `docs/personas/README.md` names exactly the files that exist. The
//! process runs from those files, so a file that drifts from the format is a
//! seat that fails to start, and a pattern renamed out of the registry is a
//! rule that silently rules nothing.
//!
//! What this can't check is the MCP half of a pattern (`Github/…`,
//! `linear.app/…`): which servers are configured is deployment state in the
//! database, not repo content. `list_personas` and `start_agent` report a
//! pattern that names no configured server (SME-241), and an unmatched
//! pattern falls to the class ceiling — never wider.

use std::sync::LazyLock;

use regex::Regex;

use crate::anthropic::tools::native_tool_definitions;
use crate::personas;
use crate::personas::rules::ToolRules;

/// Where the persona files live, and the table that must name them.
pub(crate) const PERSONA_DIR: &str = ".agents/personas/";
pub(crate) const PERSONA_TABLE: &str = "docs/personas/README.md";

/// The tracked files sitting directly under `PERSONA_DIR`: what a clone
/// records and `list_personas` reads. A subdirectory holds nobody's persona.
pub(crate) fn persona_files(tracked: &[String]) -> Vec<String> {
    tracked
        .iter()
        .filter(|path| path.strip_prefix(PERSONA_DIR).is_some_and(|rest| !rest.is_empty() && !rest.contains('/')))
        .cloned()
        .collect()
}

/// Whether a smelt-tool `pattern` names `tool`: an exact name, or the
/// pattern's `*` matching any run of characters. `personas::rules`' own glob
/// is private to that module, and it also forbids a `*` crossing a `/` —
/// neither side of a smelt-tool pattern holds a `/`, so head, middles and
/// tail in order is the same match here.
pub(crate) fn names(pattern: &str, tool: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == tool;
    }
    let mut parts = pattern.split('*').collect::<Vec<&str>>();
    let head = parts.remove(0);
    let tail = parts.pop().unwrap_or_default();
    let Some(rest) = tool.strip_prefix(head) else { return false };
    if rest.len() < tail.len() || !rest.ends_with(tail) {
        return false;
    }
    let mut rest = &rest[..rest.len() - tail.len()];
    parts.iter().all(|middle| match rest.find(middle) {
        Some(at) => {
            rest = &rest[at + middle.len()..];
            true
        }
        None => false,
    })
}

/// Each smelt-tool pattern in `rules` that names nothing the server
/// registers, as the phrase naming it: an exact name absent from
/// `registry`, or a wildcard matching none of it. A pattern with a server
/// part is this deployment's MCP configuration, so it isn't judged here.
pub(crate) fn dead_patterns(rules: &ToolRules, registry: &[String]) -> Vec<String> {
    let mut dead = Vec::new();
    for pattern in rules.allow.iter().chain(rules.deny.iter()) {
        if pattern.server.is_some() {
            continue;
        }
        let hits = |tool: &String| names(&pattern.tool, tool);
        let said = if pattern.tool.contains('*') {
            registry.iter().any(hits)
        } else {
            registry.iter().any(|tool| tool == &pattern.tool)
        };
        if !said {
            let how = if pattern.tool.contains('*') { "matches" } else { "names" };
            dead.push(format!("`{}` {how} no tool smelt registers", pattern.tool));
        }
    }
    dead
}

/// A persona table row's link to its page: `[<name>.md](<name>.md …)`.
static TABLE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([a-z0-9-]+)\.md\]\(").expect("a valid regex"));

/// The personas `docs/personas/README.md`'s table gives a page to, in the
/// order it lists them. Only table rows count: the directory holds
/// `rules.md` and its own README, which are nobody's persona.
pub(crate) fn table_personas(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
        .flat_map(|line| TABLE_LINK.captures_iter(line).map(|c| c[1].to_string()))
        .collect()
}

/// Each name `table` gives a page to that has no file in `files`, and each
/// file's name that no table row gives a page to. Both directions: a persona
/// the process names but smelt can't start, and a file nothing documents.
pub(crate) fn parity(table: &[String], files: &[String]) -> Vec<String> {
    let name_of = |path: &str| path[PERSONA_DIR.len()..].trim_end_matches(".md").to_string();
    let mut findings = Vec::new();
    for name in table {
        if !files.iter().any(|path| name_of(path) == *name) {
            findings.push(format!("`{PERSONA_TABLE}` lists {name} but {PERSONA_DIR}{name}.md is not tracked"));
        }
    }
    for path in files {
        let name = name_of(path);
        if !table.contains(&name) {
            findings.push(format!("{path} has no row in `{PERSONA_TABLE}`"));
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A persona file's text with `tools` as given, so the real parser is
    /// what builds the `ToolRules` under test.
    fn persona(tools: &str) -> String {
        format!("---\nname: x\ndescription: A fixture persona.\nsmelt:\n  tools:\n{tools}\n---\nTake the persona.\n")
    }

    fn rules_of(tools: &str) -> ToolRules {
        let text = persona(tools);
        personas::parse(".agents/personas/x.md", &text, text.len() as u64)
            .expect("the fixture parses")
            .tools
            .expect("the fixture has smelt.tools")
    }

    fn registry() -> Vec<String> {
        native_tool_definitions().into_iter().map(|def| def.name).collect()
    }

    #[test]
    fn test_a_smelt_tool_pattern_names_a_registered_tool() {
        let rules = rules_of("    class: read-only\n    allow: [read_file, start_agent]\n    deny: [edit_file]");
        assert!(dead_patterns(&rules, &registry()).is_empty(), "{rules:?}");
    }

    /// The mistake this catches: an MCP tool's name written without its
    /// server, which the grammar accepts and no rule ever matches.
    #[test]
    fn test_a_smelt_tool_pattern_naming_no_tool_is_a_finding() {
        let rules = rules_of("    allow: [merge_pull_request, linear.app/save_issue]\n");
        assert_eq!(
            dead_patterns(&rules, &registry()),
            vec!["`merge_pull_request` names no tool smelt registers".to_string()]
        );
    }

    #[test]
    fn test_a_smelt_tool_wildcard_matching_no_tool_is_a_finding() {
        // `read_*` reaches `read_file` and `read_terminal_output`; `get_*`
        // reaches nothing smelt registers of that name.
        let rules = rules_of("    allow: [read_*, get_*]\n");
        assert_eq!(dead_patterns(&rules, &registry()), vec!["`get_*` matches no tool smelt registers".to_string()]);
    }

    #[test]
    fn test_a_server_pattern_is_never_judged_here() {
        let rules = rules_of("    allow: [Github/merge_pull_request, no_such_server/get_issue]\n    deny: [no_such/*]\n");
        assert!(dead_patterns(&rules, &registry()).is_empty(), "{rules:?}");
    }

    #[test]
    fn test_the_wildcard_matches_the_tools_the_rules_name() {
        for (pattern, tool) in [
            ("read_file", "read_file"),
            ("read_*", "read_file"),
            ("read_*", "read_terminal_output"),
            ("*_file", "read_file"),
            ("g*b", "glob"),
            ("*", "todowrite"),
            ("get_*", "get_job_logs"),
            ("*_volume", "delete_volume"),
        ] {
            assert!(names(pattern, tool), "`{pattern}` should match `{tool}`");
        }
        for (pattern, tool) in [
            ("read_file", "read_files"),
            ("read_*", "write_file"),
            ("*_file", "glob"),
            ("g*o", "glob"),
            ("g*o", "grep"),
            ("list_*", "terminal_command_status"),
            ("delete_*", "read_file"),
            ("*d", "pods"),
        ] {
            assert!(!names(pattern, tool), "`{pattern}` should not match `{tool}`");
        }
    }

    #[test]
    fn test_only_files_directly_in_the_persona_dir_are_personas() {
        let tracked = [
            ".agents/personas/developer.md".to_string(),
            ".agents/personas/README.md".to_string(),
            ".agents/personas/old/architect.md".to_string(),
            ".agents/personas/".to_string(),
            ".agents/skills/x/SKILL.md".to_string(),
            "docs/personas/developer.md".to_string(),
        ];
        assert_eq!(
            persona_files(&tracked),
            vec![".agents/personas/developer.md".to_string(), ".agents/personas/README.md".to_string()]
        );
    }

    #[test]
    fn test_the_table_gives_a_page_to_each_persona() {
        let markdown = "| Persona | Works on | Moves the ticket | File |\n\
                        | -- | -- | -- | -- |\n\
                        | Developer | one Todo ticket | to In Progress | [developer.md](developer.md) |\n\
                        | Facilitator (a Claude Code sub-agent) | the retrospective | to In Review | [facilitator.md](facilitator.md) |\n\
                        \n\
                        Every persona follows [Rules every persona follows](rules.md).\n";
        assert_eq!(table_personas(markdown), vec!["developer".to_string(), "facilitator".to_string()]);
    }

    #[test]
    fn test_parity_reports_a_missing_file_and_an_undocumented_one() {
        let table = ["developer".to_string(), "facilitator".to_string()];
        let files = [".agents/personas/developer.md".to_string(), ".agents/personas/planner.md".to_string()];
        assert_eq!(
            parity(&table, &files),
            vec![
                "`docs/personas/README.md` lists facilitator but .agents/personas/facilitator.md is not tracked".to_string(),
                ".agents/personas/planner.md has no row in `docs/personas/README.md`".to_string(),
            ]
        );
        assert_eq!(parity(&table, &[".agents/personas/developer.md".to_string(), ".agents/personas/facilitator.md".to_string()]), Vec::<String>::new());
    }

    /// Every persona file the repo tracks parses the way the server parses
    /// it, and rules only over tools smelt has.
    #[test]
    fn test_every_tracked_persona_file_parses_and_rules_name_real_tools() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let tracked = super::super::tracked_files();
        let files = persona_files(&tracked);
        assert!(files.len() >= 7, "found the repo's persona files ({})", files.len());
        let registry = registry();
        let mut findings = Vec::new();
        for path in &files {
            let text = std::fs::read_to_string(root.join(path)).expect("read the persona file");
            match personas::parse(path, &text, text.len() as u64) {
                Err(e) => findings.push(e),
                Ok(persona) => {
                    if let Some(rules) = &persona.tools {
                        findings.extend(dead_patterns(rules, &registry).into_iter().map(|f| format!("{path}: {f}")));
                    }
                }
            }
        }
        assert!(findings.is_empty(), "persona files the repo can't start a seat with:\n{}", findings.join("\n"));
    }

    /// The process names its personas in one table; the seats are found by
    /// file. The two have to agree.
    #[test]
    fn test_the_persona_table_and_the_persona_files_agree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let markdown = std::fs::read_to_string(root.join(PERSONA_TABLE)).expect("read the persona table");
        let table = table_personas(&markdown);
        let files = persona_files(&super::super::tracked_files());
        assert!(table.len() >= 7, "found the persona table's rows ({})", table.len());
        let findings = parity(&table, &files);
        assert!(
            findings.is_empty(),
            "the persona docs and the persona files disagree \
             (add the file, or the row, and see docs/personas/README.md):\n{}",
            findings.join("\n")
        );
    }
}
