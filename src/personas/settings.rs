//! The `smelt` map of a persona's frontmatter (SME-506): its keys, each
//! classed by what skipping it would do, and the hand-written visitors
//! that parse it. A key this smelt doesn't know is refused at its line —
//! unless the file lists it in `smelt.ignore_if_unknown`, when its value
//! is skipped unread and its name kept for the warnings. A key that
//! restricts the persona can never be listed: an older smelt skipping it
//! would run the persona wider than its file says. The sections under a
//! key (`tools`, `resources`, …) stay strict: their own types refuse any
//! unknown key, listed or not. See docs/design/personas/persona-files.md.

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// What skipping a key would do to the seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Skipping it never widens what the seat may do.
    Skippable,
    /// It narrows what the seat may do, so no smelt may skip it.
    Restricting,
    /// Restricting, and reserved for a later ticket: not yet accepted.
    Reserved,
}

/// One `smelt` key.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Key {
    pub name: &'static str,
    pub kind: Kind,
    /// Known to every smelt that knows `ignore_if_unknown`: a file may
    /// use it unlisted. The repo lint holds every other skippable key a
    /// tracked persona uses to being listed.
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the repo lint, a test"))]
    pub baseline: bool,
}

const fn key(name: &'static str, kind: Kind, baseline: bool) -> Key {
    Key { name, kind, baseline }
}

/// Every `smelt` key, known or reserved. A new key's ticket adds its row,
/// not in the baseline: a skippable one is listed by the files that use
/// it until every smelt knows it; a restricting one lands in two merges
/// (the parser, then its use after the deploy).
pub(crate) const KEYS: &[Key] = &[
    key("model", Kind::Skippable, true),
    key("tools", Kind::Restricting, true),
    key("credentials", Kind::Restricting, true),
    key("resources", Kind::Restricting, true),
    key("compact_at", Kind::Skippable, true),
    key("ignore_if_unknown", Kind::Skippable, true),
    key("egress", Kind::Reserved, false),
    key("limits", Kind::Reserved, false),
];

/// The names this smelt accepts, as a refusal lists them.
pub(crate) fn accepted_names() -> Vec<&'static str> {
    KEYS.iter().filter(|k| k.kind != Kind::Reserved).map(|k| k.name).collect()
}

/// A `smelt` key's name as a list entry or a skipped key may be:
/// `[a-z][a-z0-9_]{0,63}`, so no other repo text reaches a warning.
pub(crate) fn is_key_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Pass 1, lenient: the frontmatter's `smelt.ignore_if_unknown` strings,
/// or none. It decides nothing — pass 2 parses the list strictly.
pub(crate) fn listed(yaml: &str) -> Vec<String> {
    let Ok(value) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
        return Vec::new();
    };
    value
        .get("smelt")
        .and_then(|smelt| smelt.get("ignore_if_unknown"))
        .and_then(serde_norway::Value::as_sequence)
        .map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The keys written directly under the frontmatter's `smelt`, in file
/// order, for the repo lint; none when it doesn't read as YAML.
#[cfg(test)]
pub(crate) fn written_keys(yaml: &str) -> Vec<String> {
    let Ok(value) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
        return Vec::new();
    };
    value
        .get("smelt")
        .and_then(serde_norway::Value::as_mapping)
        .map(|map| map.keys().filter_map(|k| k.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The skipped keys in one sentence's middle: "`smelt.a` and `smelt.b`,
/// which this smelt doesn't know"; `None` for none.
pub(crate) fn ignored_phrase(keys: &[String]) -> Option<String> {
    let names: Vec<String> = keys.iter().map(|k| format!("`smelt.{k}`")).collect();
    let (last, rest) = names.split_last()?;
    let list = if rest.is_empty() { last.clone() } else { format!("{} and {last}", rest.join(", ")) };
    Some(format!("{list}, which this smelt doesn't know"))
}

/// smelt's own settings, as the visitor read them.
#[derive(Default)]
pub(super) struct SmeltSettings {
    pub model: Option<String>,
    pub tools: Option<super::rules::RawToolRules>,
    pub credentials: Option<super::RawCredentials>,
    pub resources: Option<crate::sandbox::PodResources>,
    /// A positive count of tokens: `0`, a negative number or a string
    /// (YAML 1.2 reads `200_000` as one) is serde's error at its line.
    pub compact_at: Option<std::num::NonZeroU64>,
    /// The unknown keys skipped because the file lists them, in file order.
    pub ignored: Vec<String>,
}

/// A key under `smelt`, decided as it's read so a refusal carries the
/// key's own line.
enum Field {
    Known(&'static str),
    Ignored(String),
}

struct FieldSeed<'a>(&'a [String]);

impl<'de> DeserializeSeed<'de> for FieldSeed<'_> {
    type Value = Field;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Field, D::Error> {
        d.deserialize_identifier(self)
    }
}

impl<'de> Visitor<'de> for FieldSeed<'_> {
    type Value = Field;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a smelt setting's name")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Field, E> {
        if let Some(known) = KEYS.iter().find(|k| k.name == name && k.kind != Kind::Reserved) {
            return Ok(Field::Known(known.name));
        }
        // The shape before the list: a name that can't be listed is
        // refused like any unlisted one, never echoed into a warning.
        if is_key_name(name) && self.0.iter().any(|listed| listed == name) {
            return Ok(Field::Ignored(name.to_string()));
        }
        let expected = accepted_names().iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
        Err(E::custom(format!(
            "unknown field `{name}`, expected one of {expected} (if an older smelt may skip it, list it in \
             `smelt.ignore_if_unknown`)"
        )))
    }
}

/// `smelt.ignore_if_unknown`, parsed strictly: a list of key names, none
/// of them a restricting key's. Only checked here: pass 1 read the names.
struct IgnoreList;

struct IgnoreListVisitor;

impl<'de> Visitor<'de> for IgnoreListVisitor {
    type Value = IgnoreList;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list of smelt setting names")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<IgnoreList, A::Error> {
        while let Some(name) = seq.next_element::<String>()? {
            if !is_key_name(&name) {
                return Err(de::Error::custom(format!(
                    "{name:?} in smelt.ignore_if_unknown isn't a smelt key's name: a lowercase letter, then \
                     up to 63 lowercase letters, digits or _"
                )));
            }
            if KEYS.iter().any(|k| k.name == name && k.kind != Kind::Skippable) {
                return Err(de::Error::custom(format!(
                    "`smelt.{name}` narrows what the persona may do, so no smelt may skip it; it can't be in \
                     ignore_if_unknown"
                )));
            }
        }
        Ok(IgnoreList)
    }
}

impl<'de> Deserialize<'de> for IgnoreList {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(IgnoreListVisitor)
    }
}

/// The `smelt` value: no value is no settings, a map is read key by key.
pub(super) struct SmeltSeed<'a>(pub &'a [String]);

impl<'de> DeserializeSeed<'de> for SmeltSeed<'_> {
    type Value = Option<SmeltSettings>;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for SmeltSeed<'_> {
    type Value = Option<SmeltSettings>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a map of smelt settings")
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = SmeltSettings::default();
        let mut seen: Vec<String> = Vec::new();
        while let Some(field) = map.next_key_seed(FieldSeed(self.0))? {
            let name = match &field {
                Field::Known(name) => (*name).to_string(),
                Field::Ignored(name) => name.clone(),
            };
            if seen.contains(&name) {
                return Err(de::Error::custom(format!("duplicate field `{name}`")));
            }
            seen.push(name);
            match field {
                Field::Known("model") => out.model = map.next_value()?,
                Field::Known("tools") => out.tools = map.next_value()?,
                Field::Known("credentials") => out.credentials = map.next_value()?,
                Field::Known("resources") => out.resources = map.next_value()?,
                Field::Known("compact_at") => out.compact_at = map.next_value()?,
                Field::Known("ignore_if_unknown") => {
                    map.next_value::<Option<IgnoreList>>()?;
                }
                Field::Known(other) => {
                    return Err(de::Error::custom(format!("smelt key `{other}` has no parser: a bug in smelt")));
                }
                Field::Ignored(name) => {
                    map.next_value::<IgnoredAny>()?;
                    out.ignored.push(name);
                }
            }
        }
        Ok(Some(out))
    }
}

/// The frontmatter's top level: exactly `name`, `description` and an
/// optional `smelt`. Claude Code's own keys (`tools`, `model`, ...) are
/// refused like any other: smelt's settings go under `smelt`.
pub(super) struct Frontmatter {
    pub name: String,
    pub description: String,
    pub smelt: Option<SmeltSettings>,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum TopField {
    Name,
    Description,
    Smelt,
}

pub(super) struct FrontmatterSeed<'a>(pub &'a [String]);

impl<'de> DeserializeSeed<'de> for FrontmatterSeed<'_> {
    type Value = Frontmatter;

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Frontmatter, D::Error> {
        d.deserialize_struct("Frontmatter", &["name", "description", "smelt"], self)
    }
}

impl<'de> Visitor<'de> for FrontmatterSeed<'_> {
    type Value = Frontmatter;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("struct Frontmatter")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Frontmatter, A::Error> {
        let (mut name, mut description, mut smelt) = (None, None, None);
        while let Some(field) = map.next_key::<TopField>()? {
            match field {
                TopField::Name if name.is_some() => return Err(de::Error::duplicate_field("name")),
                TopField::Name => name = Some(map.next_value::<String>()?),
                TopField::Description if description.is_some() => {
                    return Err(de::Error::duplicate_field("description"));
                }
                TopField::Description => description = Some(map.next_value::<String>()?),
                TopField::Smelt if smelt.is_some() => return Err(de::Error::duplicate_field("smelt")),
                TopField::Smelt => smelt = Some(map.next_value_seed(SmeltSeed(self.0))?),
            }
        }
        Ok(Frontmatter {
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            description: description.ok_or_else(|| de::Error::missing_field("description"))?,
            smelt: smelt.flatten(),
        })
    }
}
