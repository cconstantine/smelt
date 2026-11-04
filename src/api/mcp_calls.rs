//! What a terminal command's scripts did with MCP tools (SME-359), in one
//! line: shown under the command in the sandbox panel and given to the
//! model by `terminal_command_status` and `read_terminal_output`. Both
//! targets build it: the panel renders it from the snapshot and from
//! `ConversationEvent::SandboxMcpCalls`.

// The web build renders it from the panel; until then only the server uses it.
#![cfg_attr(not(feature = "server"), allow(dead_code))]

use serde::{Deserialize, Serialize};

/// After this long a call still `running` is taken as lost: the agent
/// gave up on it (its 180 s wait) and its outcome is unknown.
pub const UNKNOWN_AFTER_SECS: i64 = 180;

/// The most tools the line names; the rest are counted.
const MAX_TOOLS_NAMED: usize = 4;

/// One command's (or one pod's command-less) script MCP calls.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct McpCallSummary {
    /// Every call made, the limited ones not included.
    pub calls: i64,
    /// `<server>/<tool>` and how often, most calls first.
    pub tools: Vec<(String, i64)>,
    pub refused: i64,
    pub tool_errors: i64,
    pub failed: i64,
    /// Running past `UNKNOWN_AFTER_SECS`.
    pub unknown: i64,
    /// Held back by a limit.
    pub limited: i64,
}

/// One group of recorded calls: a tool's calls with one outcome, and
/// whether a `running` one is past `UNKNOWN_AFTER_SECS`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpCallCount {
    pub server: String,
    pub tool: String,
    pub outcome: String,
    pub stale: bool,
    pub count: i64,
}

impl McpCallSummary {
    /// The summary of `counts`; `None` when there were no calls at all.
    pub fn from_counts<'a>(counts: impl IntoIterator<Item = &'a McpCallCount>) -> Option<Self> {
        let mut summary = Self::default();
        let mut tools: Vec<(String, i64)> = Vec::new();
        for count in counts {
            if count.outcome == "limited" {
                summary.limited += count.count;
                continue;
            }
            summary.calls += count.count;
            match count.outcome.as_str() {
                "refused" => summary.refused += count.count,
                "tool_error" => summary.tool_errors += count.count,
                "failed" => summary.failed += count.count,
                "running" if count.stale => summary.unknown += count.count,
                _ => {}
            }
            let name = format!("{}/{}", count.server, count.tool);
            match tools.iter_mut().find(|(tool, _)| *tool == name) {
                Some((_, n)) => *n += count.count,
                None => tools.push((name, count.count)),
            }
        }
        if summary.calls == 0 && summary.limited == 0 {
            return None;
        }
        tools.sort_by(|(a, n), (b, m)| m.cmp(n).then_with(|| a.cmp(b)));
        summary.tools = tools;
        Some(summary)
    }

    /// The line itself, e.g. `MCP: 14 calls · linear/list_issues ×12,
    /// linear/save_comment ×2 · 1 refused`.
    pub fn text(&self) -> String {
        let plural = |n: i64, one: &str, many: &str| if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") };
        let mut named: Vec<String> = self.tools.iter().take(MAX_TOOLS_NAMED).map(|(tool, n)| format!("{tool} ×{n}")).collect();
        let others = self.tools.len().saturating_sub(MAX_TOOLS_NAMED) as i64;
        if others > 0 {
            named.push(plural(others, "other tool", "other tools"));
        }
        let mut parts = vec![format!("MCP: {}", plural(self.calls, "call", "calls"))];
        if !named.is_empty() {
            parts.push(named.join(", "));
        }
        for (n, one, many) in [
            (self.refused, "refused", "refused"),
            (self.tool_errors, "tool error", "tool errors"),
            (self.failed, "failed", "failed"),
            (self.unknown, "outcome unknown", "outcome unknown"),
            (self.limited, "held back by a limit", "held back by a limit"),
        ] {
            if n > 0 {
                parts.push(plural(n, one, many));
            }
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(tool: &str, outcome: &str, stale: bool, count: i64) -> McpCallCount {
        McpCallCount { server: "linear".into(), tool: tool.into(), outcome: outcome.into(), stale, count }
    }

    #[test]
    fn test_the_line_counts_calls_by_tool_most_first_with_what_went_wrong() {
        let counts = [
            count("save_comment", "ok", false, 2),
            count("list_issues", "ok", false, 11),
            count("list_issues", "refused", false, 1),
            count("delete_issue", "limited", false, 3),
        ];
        let summary = McpCallSummary::from_counts(&counts).expect("calls");
        assert_eq!(summary.calls, 14);
        assert_eq!(summary.text(), "MCP: 14 calls · linear/list_issues ×12, linear/save_comment ×2 · 1 refused · 3 held back by a limit");
    }

    #[test]
    fn test_a_running_call_past_the_agents_wait_reads_as_outcome_unknown() {
        let counts = [count("save_comment", "running", true, 1), count("save_comment", "running", false, 1), count("get", "tool_error", false, 1), count("get", "failed", false, 2)];
        let text = McpCallSummary::from_counts(&counts).expect("calls").text();
        assert_eq!(text, "MCP: 5 calls · linear/get ×3, linear/save_comment ×2 · 1 tool error · 2 failed · 1 outcome unknown");
    }

    #[test]
    fn test_one_call_and_many_tools_read_naturally() {
        assert_eq!(McpCallSummary::from_counts(&[count("get", "ok", false, 1)]).expect("a call").text(), "MCP: 1 call · linear/get ×1");
        let many: Vec<_> = (0..6).map(|i| count(&format!("t{i}"), "ok", false, 6 - i)).collect();
        assert_eq!(
            McpCallSummary::from_counts(&many).expect("calls").text(),
            "MCP: 21 calls · linear/t0 ×6, linear/t1 ×5, linear/t2 ×4, linear/t3 ×3, 2 other tools"
        );
        assert_eq!(McpCallSummary::from_counts(&[]), None);
    }
}
