//! The grammar of a persona's `smelt.tools` (SME-241): a class ceiling and
//! allow/deny patterns, as `docs/design/personas/tool-rules.md#the-grammar`
//! spells them. `check` turns the YAML's raw text into `ToolRules`; a
//! pattern that can't be one is an error naming the persona file.
//! `RuleChain` evaluates the chain a conversation stores against a tool
//! offer and a tool call — the one evaluation SME-241 enforces everywhere.

use crate::anthropic::tools::ToolClass;
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

impl Ceiling {
    /// The design's spelling, as the refusal message quotes it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Ceiling::All => "all",
            Ceiling::NoDestructive => "no-destructive",
            Ceiling::ReadOnly => "read-only",
        }
    }

    /// The ceiling with no pattern matching: the class at or below it.
    fn allows(self, class: ToolClass) -> bool {
        match self {
            Ceiling::All => true,
            Ceiling::NoDestructive => class != ToolClass::Destructive,
            Ceiling::ReadOnly => class == ToolClass::ReadOnly,
        }
    }
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

/// What one rule entry judges: a smelt tool by name, or an MCP tool by
/// its server and the name that server lists it under — the same split
/// `crate::mcp::parse_tool_name` does.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ToolRef<'a> {
    Native(&'a str),
    Mcp { server: &'a str, tool: &'a str },
}

impl ToolRef<'_> {
    /// The name the model calls it by, as the refusal quotes it.
    fn name(&self) -> String {
        match self {
            ToolRef::Native(name) => name.to_string(),
            ToolRef::Mcp { server, tool } => crate::mcp::namespaced_tool_name(server, tool),
        }
    }
}

/// A conversation's effective rules: the `agents.rule_chain` array read
/// back, oldest entry first. Evaluation is a conjunction over entries —
/// each narrows what came before, and nothing re-reads persona files
/// (docs/design/personas/tool-rules.md#narrowing).
#[derive(Clone, Debug, Default)]
pub(crate) struct RuleChain(Vec<ToolRules>);

impl RuleChain {
    /// The stored column: absent or NULL is the empty chain — allow-all,
    /// the user's own conversation as today. JSON that won't read as
    /// entries is possible only if the store was corrupted behind smelt's
    /// back (it's written by typed serde): fall open with a warning, the
    /// same way the call path reads, so offer and call stay a consistent
    /// pair rather than offering tools whose calls are refused.
    pub(crate) fn from_stored(stored: Option<serde_json::Value>) -> RuleChain {
        let entries = match stored
            .map(serde_json::from_value::<Vec<ToolRules>>)
            .transpose()
        {
            Ok(entries) => entries.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "an agents.rule_chain value won't read as tool rules; its conversation is left unruled");
                Vec::new()
            }
        };
        RuleChain(entries)
    }

    /// Whether the tool may be called at `class`: every entry must allow
    /// it. The Err names the deciding rule and the tool.
    pub(crate) fn allows_at(&self, tool: ToolRef, class: ToolClass) -> Result<(), String> {
        for entry in &self.0 {
            entry.allows(tool, class)?;
        }
        Ok(())
    }

    /// Whether to offer the tool: allowed at some class it can have
    /// (`Class::can_be` — only `http_request` has more than one).
    pub(crate) fn allows_any(&self, tool: ToolRef, classes: &[ToolClass]) -> bool {
        classes.iter().any(|class| self.allows_at(tool, *class).is_ok())
    }
}

impl ToolRules {
    /// One entry's verdict: the most specific matching pattern decides,
    /// and a deny wins a tie at that level. Nothing matches: the class
    /// against the ceiling — specificity level 4 in the design.
    fn allows(&self, tool: ToolRef, class: ToolClass) -> Result<(), String> {
        let mut best: Option<(u8, &Pattern, bool)> = None; // level, pattern, denies?
        for (patterns, denies) in [(&self.allow, false), (&self.deny, true)] {
            for pattern in patterns {
                if !pattern.matches(tool) {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((best_level, _, best_denies)) => {
                        pattern.level() < best_level
                            || (pattern.level() == best_level && denies && !best_denies)
                    }
                };
                if better {
                    best = Some((pattern.level(), pattern, denies));
                }
            }
        }
        match best {
            Some((_, pattern, true)) => Err(format!(
                "{} is denied by this conversation's tool rules (deny pattern `{}`)",
                tool.name(),
                String::from(pattern.clone())
            )),
            Some((_, _, false)) => Ok(()),
            None if self.class.allows(class) => Ok(()),
            None => Err(format!(
                "{} is denied by this conversation's tool rules (class ceiling {} doesn't allow a {} tool)",
                tool.name(),
                self.class.as_str(),
                class.as_str()
            )),
        }
    }

    /// The patterns naming a server `server_names` has no match for: what
    /// a renamed or removed server leaves behind, a rule that silently
    /// rules nothing. Never an error — a pattern may name a server that
    /// isn't configured yet — so this is a report, made where the rules
    /// are read (SME-241: `list_personas`, and `start_agent` to the parent).
    /// Allow's patterns first, then deny's, each in the order written.
    pub(crate) fn unmatched_servers(&self, server_names: &[String]) -> Vec<String> {
        self.allow
            .iter()
            .chain(self.deny.iter())
            .filter(|p| match &p.server {
                // A smelt-tool pattern names no server, so it's never
                // unmatched; a server pattern is unmatched exactly when no
                // configured server has that name — the same exact match
                // `matches` makes when it rules.
                None => false,
                Some(server) => !server_names.iter().any(|name| name == server),
            })
            .map(|p| String::from(p.clone()))
            .collect()
    }
}

/// The report both readers say, in one sentence: which patterns name no
/// configured server. Each caller says whose patterns they are — the
/// persona in `list_personas`, the child in `start_agent`'s notice — so
/// this part stays the same words everywhere (SME-241).
pub(crate) fn unmatched_note(patterns: &[String]) -> Option<String> {
    match patterns.len() {
        0 => None,
        1 => Some(format!(
            "smelt.tools pattern `{}` names no configured server, so it rules nothing (a renamed server leaves this behind)",
            patterns[0]
        )),
        _ => Some(format!(
            "smelt.tools patterns {} name no configured server, so they rule nothing (a renamed server leaves this behind)",
            patterns.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ")
        )),
    }
}

impl Pattern {
    /// The design's specificity: 1 exact, 2 a wildcard in the tool name,
    /// 3 a whole server (`<server>/*`). Computed, never stored.
    fn level(&self) -> u8 {
        if self.server.is_some() && self.tool == "*" {
            3
        } else if self.tool.contains('*') {
            2
        } else {
            1
        }
    }

    /// Whether the pattern names this tool. A smelt-tool pattern never
    /// carries a server part, and vice versa: an MCP tool is named by its
    /// server exactly (a wildcard there is refused by the grammar) plus a
    /// pattern in the tool name.
    fn matches(&self, tool: ToolRef) -> bool {
        match (tool, &self.server) {
            (ToolRef::Native(name), None) => glob(&self.tool, name),
            (ToolRef::Mcp { server, tool }, Some(pattern_server)) => pattern_server == server && glob(&self.tool, tool),
            _ => false,
        }
    }
}

/// The pattern's `*` matches any run of characters with no `/` in it —
/// a wildcard can't reach across a separator into another server's name.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => {
            let Some(rest) = text.strip_prefix(head) else { return false };
            (0..=rest.len()).any(|cut| {
                rest.get(cut..).is_some() && !rest[..cut].contains('/') && glob(tail, &rest[cut..])
            })
        }
    }
}
