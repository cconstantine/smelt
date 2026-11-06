//! The Claude Code CLI provider kind (SME-399): turns that run through the
//! official, unmodified `claude` binary on the server host, signed in to
//! the user's Claude subscription, so the subscription pays.
//!
//! This part (SME-429) is the binary, its status and its environment: the
//! provider page shows what `claude --version` and `claude auth status`
//! say, and every process smelt starts gets a cleared environment with an
//! allowlist (`cli_env`). SME-430's transport runs the turns: `bridge`
//! shows smelt's tools to the CLI, `run` holds a turn's `claude -p`
//! process, and `render` says what of smelt's history it's sent.
//!
//! The wire types here cross to the browser; the rest is server-only.

use serde::{Deserialize, Serialize};

/// The binary a new Claude Code provider runs: `claude` on the server's
/// `PATH` (the server image puts it at `/opt/claude/bin/claude`).
pub const DEFAULT_BINARY: &str = "claude";

/// The error a Claude Code turn ends with when its conversation no longer
/// fits the model's window: smelt compacts these from SME-431 on.
#[cfg(feature = "server")]
pub const CANT_COMPACT_YET: &str = "This conversation no longer fits its Claude Code model's context window, and smelt can't compact a Claude Code conversation yet. Start a new conversation, or switch this one to another provider's model.";

/// Anthropic's terms for this route, quoted on the provider form: the
/// kind is the one that breaks if they change (SME-399's risk).
pub const POLICY_QUOTE: &str = "Individuals may \u{201c}sign in to the unmodified Claude Code binary with your own Claude subscription, including where a platform hosts Claude Code.\u{201d}";

/// Where `POLICY_QUOTE` comes from.
pub const POLICY_SOURCE: &str = "Claude Code's legal and compliance page, as of November 2026";

/// What the provider page shows about the CLI. It never holds a token or
/// anything else the CLI printed beyond its version line and two short
/// words of `auth status` (`auth_method`, `plan`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ClaudeCliStatus {
    /// `claude --version`'s first line, such as `2.1.318 (Claude Code)`.
    pub version: Option<String>,
    pub signed_in: bool,
    /// `auth status`'s `authMethod`: `oauth_token` for
    /// `CLAUDE_CODE_OAUTH_TOKEN`, `claude.ai` for a `/login`.
    pub auth_method: Option<String>,
    /// `auth status`'s `subscriptionType`, such as `max`.
    pub plan: Option<String>,
    /// Why the CLI couldn't be run or read, if it couldn't.
    pub error: Option<String>,
}

impl ClaudeCliStatus {
    /// The provider page's status line.
    pub fn summary(&self) -> String {
        let Some(version) = &self.version else {
            let why = self.error.as_deref().unwrap_or("it printed no version.");
            return format!("Couldn't run Claude Code: {why}");
        };
        if let Some(error) = &self.error {
            return format!("Claude Code {version}: couldn't read its sign-in: {error}");
        }
        if !self.signed_in {
            return format!(
                "Claude Code {version}: not signed in. Run claude setup-token on your own machine, set what it prints as CLAUDE_CODE_OAUTH_TOKEN in the smelt server's environment, and restart the server."
            );
        }
        let how = match self.auth_method.as_deref() {
            Some("oauth_token") => " with CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            Some("claude.ai") => " with a claude.ai login".to_string(),
            Some(method) => format!(" with {method}"),
            None => String::new(),
        };
        match &self.plan {
            Some(plan) => format!("Claude Code {version}: signed in{how}, on the {plan} plan."),
            None => format!("Claude Code {version}: signed in{how}; it didn't say which plan."),
        }
    }
}

#[cfg(feature = "server")]
pub mod bridge;
#[cfg(feature = "claude-cli-capture")]
pub mod capture;
#[cfg(feature = "server")]
pub mod errors;
#[cfg(feature = "server")]
pub mod files;
#[cfg(feature = "server")]
pub mod process;
#[cfg(feature = "server")]
pub mod render;
#[cfg(feature = "server")]
pub mod run;
#[cfg(feature = "server")]
mod server;

#[cfg(feature = "server")]
pub use server::*;

#[cfg(all(test, feature = "server"))]
pub(crate) mod tests;
