//! The grammar of a persona's `smelt.tools` (SME-241): a class ceiling and
//! allow/deny patterns, as `docs/design/personas/tool-rules.md#the-grammar`
//! spells them. `check` turns the YAML's raw text into `ToolRules`; a
//! pattern that can't be one is an error naming the persona file.
//! Evaluation of a rule chain against a tool call is SME-241's next step.

use serde::{Deserialize, Serialize};

/// The class ceiling: which classes the rules allow unless a pattern says
/// otherwise — exactly the design's three spellings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Ceiling {
    #[default]
    All,
    NoDestructive,
    ReadOnly,
}

/// A parsed tool pattern: `write_file` names a smelt tool, `github/create_*`
/// a pattern in one server's tool names, `github/*` that whole server. The
/// `tool` segment may hold `*`; the `server` segment, when present, never
/// does (checked, so it's an invariant).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub(crate) struct Pattern {
    pub(crate) server: Option<String>,
    pub(crate) tool: String,
}

impl From<Pattern> for String {
    fn from(p: Pattern) -> String {
        match &p.server {
            Some(server) => format!("{server}/{}", p.tool),
            None => p.tool.clone(),
        }
    }
}

/// Reading a pattern back from a chain row smelt wrote itself: the split,
/// with no re-checking (the grammar was checked when the persona parsed;
/// a corrupt chain falls open the way SME-241's plan has it).
impl From<String> for Pattern {
    fn from(text: String) -> Pattern {
        match text.split_once('/') {
            Some((server, tool)) => Pattern { server: Some(server.to_string()), tool: tool.to_string() },
            None => Pattern { server: None, tool: text },
        }
    }
}

/// One persona's rules, as stored and evaluated: the class ceiling and the
/// two pattern lists. Only `check` builds one, so the patterns hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolRules {
    #[serde(default)]
    pub(crate) class: Ceiling,
    #[serde(default)]
    pub(crate) allow: Vec<Pattern>,
    #[serde(default)]
    pub(crate) deny: Vec<Pattern>,
}

/// The YAML shape, before its patterns are checked: lists as text.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RawToolRules {
    class: Ceiling,
    allow: Vec<String>,
    deny: Vec<String>,
}

/// Turn the YAML's rules into checked ones. Every error starts with
/// `path`, the persona file, like every other parse error. The checks are
/// the design's four (tool-rules.md#the-grammar): empty patterns, more
/// than one `/`, a `*` in a server segment, and a pattern text in both
/// lists. A pattern naming a server that isn't configured is none of
/// them — that's a report, not an error.
pub(crate) fn check(path: &str, raw: RawToolRules) -> Result<ToolRules, String> {
    let parse = |text: String| -> Result<Pattern, String> {
        if text.is_empty() {
            return Err(format!("{path}: smelt.tools has an empty pattern: name a smelt tool or a <server>/<tool> pair."));
        }
        match text.split_once('/') {
            Some((server, tool)) if server.contains('/') || tool.contains('/') => Err(format!(
                "{path}: smelt.tools pattern {text:?} has more than one /: it names a smelt tool, one server's tools, or one tool in one server."
            )),
            // `*/get_*` would let a rule reach every server's reads; the
            // design keeps server rules exact so a server rule can't be
            // undercut by a pattern that names no server.
            Some((server, _)) if server.contains('*') => Err(format!(
                "{path}: smelt.tools pattern {text:?} has a wildcard in its server part: a pattern's server is always named."
            )),
            Some((server, tool)) if server.is_empty() || tool.is_empty() => Err(format!(
                "{path}: smelt.tools pattern {text:?} has an empty part: name both the server and the tool, or neither."
            )),
            Some((server, tool)) => Ok(Pattern { server: Some(server.to_string()), tool: tool.to_string() }),
            None => Ok(Pattern { server: None, tool: text }),
        }
    };
    let allow = raw.allow.iter().cloned().map(&parse).collect::<Result<Vec<_>, _>>()?;
    let deny = raw.deny.iter().cloned().map(&parse).collect::<Result<Vec<_>, _>>()?;
    if let Some(dup) = raw.deny.iter().find(|text| raw.allow.iter().any(|a| a == *text)) {
        return Err(format!("{path}: smelt.tools pattern {dup:?} is in both allow and deny: it belongs to one."));
    }
    Ok(ToolRules { class: raw.class, allow, deny })
}
