//! Every tool's class (SME-237): what a call does outside the agent's own
//! sandbox and conversation, on one scale for smelt's tools and MCP tools
//! (`docs/design/personas/tool-rules.md#classes`). A persona's tool rules
//! (SME-241) allow or deny by it; nothing filters by it yet, and a call's
//! class rides on its `tool` span as `smelt.tool.class`.

use serde_json::Value;

/// A tool call's class, least to most reaching: a persona's ceiling allows
/// a call when `class <= ceiling` (SME-241), so the order matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ToolClass {
    /// Changes nothing outside the agent's own sandbox and conversation.
    ReadOnly,
    /// Changes something outside, additively or reversibly.
    Writing,
    /// Can delete or overwrite something outside.
    Destructive,
}

impl ToolClass {
    /// The design's spelling, as on the span and in persona files.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ToolClass::ReadOnly => "read-only",
            ToolClass::Writing => "writing",
            ToolClass::Destructive => "destructive",
        }
    }
}

/// A native tool's class, beside its definition (`Tool { def, run, class }`).
/// The field is required, so a tool without one doesn't compile, and
/// `class::tests` holds every tool's class as the design writes it.
pub(super) enum Class {
    Fixed(ToolClass),
    /// The class depends on the call's input; `can_be` lists every class
    /// `of` can return.
    ByInput { can_be: &'static [ToolClass], of: fn(&Value) -> ToolClass },
}

impl Class {
    pub(super) const READ_ONLY: Class = Class::Fixed(ToolClass::ReadOnly);
    pub(super) const WRITING: Class = Class::Fixed(ToolClass::Writing);
    /// `http_request`'s: by the method it sends.
    pub(super) const BY_HTTP_METHOD: Class = Class::ByInput {
        can_be: &[ToolClass::ReadOnly, ToolClass::Writing, ToolClass::Destructive],
        of: http_request_class,
    };

    /// The class of one call with `input`.
    pub(super) fn of_call(&self, input: &Value) -> ToolClass {
        match self {
            Class::Fixed(class) => *class,
            Class::ByInput { of, .. } => of(input),
        }
    }

    /// Every class a call to this tool can have: what the offered list
    /// is judged by (SME-241).
    #[cfg_attr(not(test), expect(dead_code, reason = "SME-241 offers tools by it"))]
    pub(super) fn can_be(&self) -> &[ToolClass] {
        match self {
            Class::Fixed(class) => std::slice::from_ref(class),
            Class::ByInput { can_be, .. } => can_be,
        }
    }
}

/// `http_request`'s class, from the method it would send, read and parsed
/// as the tool reads and parses it (`requested_method`, `parse_method`):
/// `GET`, `HEAD` and `OPTIONS` read-only, `DELETE` destructive, any other
/// writing, one that doesn't parse included (nothing is sent then). A
/// redirect can't raise it: every redirect keeps the method but a 303,
/// which makes it a `GET`.
fn http_request_class(input: &Value) -> ToolClass {
    use reqwest::Method;
    match crate::http_request::parse_method(super::web::requested_method(input)) {
        Ok(method) if [Method::GET, Method::HEAD, Method::OPTIONS].contains(&method) => ToolClass::ReadOnly,
        Ok(Method::DELETE) => ToolClass::Destructive,
        _ => ToolClass::Writing,
    }
}

#[cfg(test)]
mod tests;
