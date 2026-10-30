//! Persona files (SME-235): the only code that knows their format.
//! A persona file is `.agents/personas/<name>.md` in a repo: YAML
//! frontmatter holding exactly `name`, `description` and an optional
//! `smelt` map of smelt's own settings, then the instructions as Markdown.
//! Any other key, at the top or under `smelt`, is refused: a field arrives
//! with the ticket that uses it. See docs/design/personas/persona-files.md.

use serde::Deserialize;

pub(crate) mod frontmatter;
pub(crate) mod links;
pub(crate) mod rules;

/// A parsed persona file.
#[derive(Clone, Debug, PartialEq)]
pub struct Persona {
    /// Equal to the file's name without `.md`.
    pub name: String,
    pub description: String,
    /// The body, as written: its relative links are rewritten only when a
    /// persona is started (SME-236).
    pub instructions: String,
    /// `smelt.model`: a model name, never a provider.
    pub model: Option<String>,
    /// `smelt.tools`: the persona's tool rules, checked against the grammar
    /// (SME-241). `None` allows everything, as before the field existed.
    pub tools: Option<rules::ToolRules>,
    /// `smelt.credentials`: the credential names this persona runs with
    /// (SME-242). `None` — and `Some(vec![])` — mean none: a field that
    /// is absent grants nothing, exactly as one that lists nothing.
    pub credentials: Option<Vec<String>>,
}

/// The frontmatter. Claude Code's own keys (`tools`, `model`, ...) are
/// refused like any other: smelt's settings go under `smelt`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    name: String,
    description: String,
    /// `smelt:` with no value is the same as no `smelt` key.
    #[serde(default)]
    smelt: Option<SmeltSettings>,
}

/// smelt's own settings. `credentials` arrived with SME-242; later tickets
/// add `egress` (SME-243) and `limits` (SME-245).
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SmeltSettings {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Option<rules::RawToolRules>,
    #[serde(default)]
    credentials: Option<RawCredentials>,
}

/// `smelt.credentials`: a list of credential names, each checked where
/// it is read rather than after — so a bad entry is an error at the
/// line the YAML parser stands on, like serde's own. A name that isn't
/// a stored secret is not an error here (the store is the deployment's,
/// not the repo's); it is reported at start and in `list_personas`
/// instead (D6).
#[derive(Debug)]
struct RawCredentials(Vec<String>);

struct CredentialsVisitor;

impl<'de> serde::de::Visitor<'de> for CredentialsVisitor {
    type Value = RawCredentials;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list of credential names")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut names = Vec::new();
        while let Some(name) = seq.next_element::<String>()? {
            if !crate::credentials::is_credential_name(&name) {
                // serde_norway appends its location to this.
                return Err(serde::de::Error::custom(format!(
                    "`{name}` isn't a credential name: 1 to 64 letters, digits, - or _"
                )));
            }
            names.push(name);
        }
        Ok(RawCredentials(names))
    }
}

impl<'de> Deserialize<'de> for RawCredentials {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(CredentialsVisitor)
    }
}

/// The largest persona file: `AGENTS.md`'s cap, since it's read the same
/// way (`read_instructions_file` returns only that much). A larger file is
/// refused rather than run cut short.
const MAX_FILE_BYTES: usize = crate::git::INSTRUCTIONS_MAX_BYTES;

/// A persona's name: 1 to 64 of `a-z`, `0-9` and `-`, not starting or
/// ending with `-` (the Agent Skills rule for `name`).
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether a file in `.agents/personas/` named `file_name` could hold a
/// persona: `<name>.md` with a valid name, since `name` must equal it.
/// Others are never recorded, so a file name's text (which an untrusted
/// repo chooses) reaches the system prompt only in this shape.
pub fn is_persona_file_name(file_name: &str) -> bool {
    file_name.strip_suffix(".md").is_some_and(valid_name)
}

/// serde's error, at the file's line: the frontmatter starts on the
/// file's line 2. Its own " at line L column C" is dropped, as it counts
/// from the frontmatter.
fn yaml_error(path: &str, error: &serde_norway::Error) -> String {
    let message = error.to_string();
    match error.location() {
        Some(at) => {
            let own = format!(" at line {} column {}", at.line(), at.column());
            let message = message.replace(&own, "");
            format!("{path}, line {} column {}: {message}", at.line() + 1, at.column())
        }
        None => format!("{path}: {message}"),
    }
}

/// Parses the persona file at `path` (relative to its checkout, e.g.
/// `.agents/personas/developer.md`), `file_bytes` long in all, of which
/// `text` is what was read. Every error starts with `path`.
pub fn parse(path: &str, text: &str, file_bytes: u64) -> Result<Persona, String> {
    if file_bytes > MAX_FILE_BYTES as u64 {
        return Err(format!("{path} is {file_bytes} bytes: a persona file may be at most {MAX_FILE_BYTES}."));
    }
    let (yaml, body) = frontmatter::split(text).map_err(|e| format!("{path} {e}"))?;
    let front: Frontmatter = serde_norway::from_str(yaml).map_err(|e| yaml_error(path, &e))?;
    if !valid_name(&front.name) {
        return Err(format!(
            "{path}: `name` {:?} must be 1 to 64 lowercase letters, digits and -, not starting or ending with -.",
            front.name
        ));
    }
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if file_name.strip_suffix(".md") != Some(front.name.as_str()) {
        return Err(format!("{path}: `name` is `{}` but the file is `{file_name}`: they must match.", front.name));
    }
    let description_chars = front.description.chars().count();
    if front.description.trim().is_empty() || description_chars > 1024 {
        return Err(format!(
            "{path}: `description` must be 1 to 1024 characters (it's {description_chars})."
        ));
    }
    let smelt = front.smelt.unwrap_or_default();
    let model = smelt.model;
    if model.as_deref().is_some_and(|m| m.trim().is_empty()) {
        return Err(format!("{path}: `smelt.model` is empty: name a model, or leave it out."));
    }
    let tools = smelt.tools.map(|raw| rules::check(path, raw)).transpose()?;
    let credentials = smelt.credentials.map(|raw| raw.0);
    Ok(Persona {
        name: front.name,
        description: front.description,
        instructions: body.to_string(),
        model,
        tools,
        credentials,
    })
}

#[cfg(test)]
mod tests;
