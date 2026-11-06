//! Turns the capture's raw output into the fixtures the fake CLI
//! replays: every id becomes a stable fake one, and every date, address,
//! path, signature and email a fixed stand-in. Run by
//! `scripts/claude-cli-capture <out dir> sanitize`; nothing raw is committed.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

/// The scenarios whose output becomes a fixture.
const CAPTURED: [&str; 9] = ["loop", "kill", "resume", "bad_token", "bad_model", "capped", "thinking_off", "pending", "missing"];

#[derive(Default)]
struct Ids(HashMap<String, String>);

impl Ids {
    fn stand_in(&mut self, kind: &str, real: &str) -> String {
        let n = self.0.len() + 1;
        self.0
            .entry(real.to_string())
            .or_insert_with(|| match kind {
                "uuid" => format!("00000000-0000-4000-8000-{n:012}"),
                prefix => format!("{prefix}fixture{n:04}"),
            })
            .clone()
    }
}

struct Scrub {
    ids: Ids,
    regexes: Vec<(regex::Regex, &'static str)>,
    uuid: regex::Regex,
    out_dir: String,
    /// `out_dir` as the CLI names a project directory after it: every
    /// character but a letter or digit made `-` (review 1 L5).
    out_slug: String,
    home: String,
}

impl Scrub {
    fn new(out_dir: &Path) -> Result<Self, String> {
        let regexes = [
            (r"[\w.+-]+@[\w-]+(\.[\w-]+)+", "user@example.com"),
            (r"20\d\d-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z", "2026-01-01T00:00:00.000Z"),
            (r"127\.0\.0\.1:\d+", "127.0.0.1:1"),
        ]
        .into_iter()
        .map(|(pattern, with)| regex::Regex::new(pattern).map(|re| (re, with)).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
        let uuid = regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").map_err(|e| e.to_string())?;
        Ok(Self {
            ids: Ids::default(),
            regexes,
            uuid,
            out_dir: out_dir.display().to_string(),
            out_slug: out_dir.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect(),
            home: std::env::var("HOME").unwrap_or_default(),
        })
    }

    fn string(&mut self, key: &str, text: &str) -> Value {
        if (key == "signature" || key == "signature_delta") && !text.is_empty() {
            return Value::String("fixture-signature".to_string());
        }
        // A socket named after the CLI's pid (review 1 L5).
        if key == "messaging_socket_path" {
            return Value::String("/tmp/cc-socks/1.sock".to_string());
        }
        if self.uuid.is_match(text) {
            return Value::String(self.ids.stand_in("uuid", text));
        }
        for prefix in ["msg_", "req_", "toolu_"] {
            if text.starts_with(prefix) && text.len() > prefix.len() + 8 && !text.contains(' ') {
                return Value::String(self.ids.stand_in(prefix, text));
            }
        }
        let mut text = text.replace(&self.out_dir, "/capture").replace(&self.out_slug, "-capture");
        if !self.home.is_empty() {
            text = text.replace(&self.home, "/home/user");
        }
        for (pattern, with) in &self.regexes {
            text = pattern.replace_all(&text, *with).into_owned();
        }
        Value::String(text)
    }

    fn value(&mut self, key: &str, value: &Value) -> Value {
        match value {
            Value::String(text) => self.string(key, text),
            Value::Number(_) if key == "resetsAt" => Value::from(1_767_225_600_u64),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.value(key, item)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), self.value(k, v))).collect()),
            other => other.clone(),
        }
    }
}

/// Writes `<fixtures>/<name>.jsonl` for each captured scenario found in
/// `out_dir`: one sanitized CLI line each, timings dropped.
pub(super) fn all(out_dir: &Path, fixtures: &Path) -> Result<(), String> {
    std::fs::create_dir_all(fixtures).map_err(|e| e.to_string())?;
    for name in CAPTURED {
        let Ok(raw) = std::fs::read_to_string(out_dir.join(format!("{name}.raw.jsonl"))) else {
            continue;
        };
        let mut scrub = Scrub::new(out_dir)?;
        let mut lines = String::new();
        for line in raw.lines() {
            let value: Value = serde_json::from_str(line).map_err(|e| format!("{name}: a raw line isn't JSON: {e}"))?;
            let event = &value["e"];
            if event.get("unparsed").is_some() {
                continue;
            }
            lines.push_str(&scrub.value("", event).to_string());
            lines.push('\n');
        }
        std::fs::write(fixtures.join(format!("{name}.jsonl")), lines).map_err(|e| e.to_string())?;
    }
    Ok(())
}
