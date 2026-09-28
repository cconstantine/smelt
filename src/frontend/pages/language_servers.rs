use std::collections::BTreeMap;

use dioxus::prelude::*;

use crate::api::language_servers::{
    create_language_server, delete_language_server, get_language_server, list_language_servers,
    lookup_language_server, update_language_server,
};
use crate::frontend::Route;
use crate::models::{LanguageServer, LanguageServerConfig};

/// The non-empty, trimmed lines of a text field.
fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// `key = value` lines into a map; a line without `=` is an error naming it.
fn pairs(text: &str, what: &str) -> Result<BTreeMap<String, String>, String> {
    lines(text)
        .into_iter()
        .map(|line| match line.split_once('=') {
            Some((key, value)) if !key.trim().is_empty() => Ok((key.trim().to_string(), value.trim().to_string())),
            _ => Err(format!("{what}: {line:?} should look like key = value.")),
        })
        .collect()
}

fn pairs_text(map: &BTreeMap<String, String>) -> String {
    map.iter().map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("\n")
}

/// A JSON field: empty is none, anything else must parse.
fn json_field(text: &str, what: &str) -> Result<Option<serde_json::Value>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(text).map(Some).map_err(|e| format!("{what} isn't valid JSON: {e}"))
}

fn json_text(value: &Option<serde_json::Value>) -> String {
    value.as_ref().map(|v| serde_json::to_string_pretty(v).unwrap_or_default()).unwrap_or_default()
}

/// The form's fields as text, and back.
#[derive(Clone, Debug, Default, PartialEq)]
struct FormText {
    name: String,
    image: String,
    install_command: String,
    command: String,
    args: String,
    env: String,
    file_types: String,
    root_markers: String,
    initialization_options: String,
    settings: String,
    memory_limit: String,
    cpu_limit: String,
    enabled: bool,
}

impl FormText {
    fn from_config(config: &LanguageServerConfig) -> Self {
        FormText {
            name: config.name.clone(),
            image: config.image.clone(),
            install_command: config.install_command.clone(),
            command: config.command.clone(),
            args: config.args.join("\n"),
            env: pairs_text(&config.env),
            file_types: pairs_text(&config.file_types),
            root_markers: config.root_markers.join("\n"),
            initialization_options: json_text(&config.initialization_options),
            settings: json_text(&config.settings),
            memory_limit: config.memory_limit.clone(),
            cpu_limit: config.cpu_limit.clone(),
            enabled: config.enabled,
        }
    }

    fn to_config(&self) -> Result<LanguageServerConfig, String> {
        Ok(LanguageServerConfig {
            name: self.name.trim().to_string(),
            image: self.image.trim().to_string(),
            install_command: self.install_command.trim().to_string(),
            command: self.command.trim().to_string(),
            args: lines(&self.args),
            env: pairs(&self.env, "Environment")?,
            file_types: pairs(&self.file_types, "File types")?
                .into_iter()
                .map(|(ext, lang)| (ext.trim_start_matches('.').to_string(), lang))
                .collect(),
            root_markers: lines(&self.root_markers),
            initialization_options: json_field(&self.initialization_options, "Initialization options")?,
            settings: json_field(&self.settings, "Settings")?,
            memory_limit: self.memory_limit.trim().to_string(),
            cpu_limit: self.cpu_limit.trim().to_string(),
            enabled: self.enabled,
        })
    }
}

/// What a new server's form starts with.
fn blank() -> LanguageServerConfig {
    LanguageServerConfig {
        memory_limit: "2Gi".to_string(),
        cpu_limit: "1".to_string(),
        enabled: true,
        ..Default::default()
    }
}

#[component]
pub fn LanguageServersIndex() -> Element {
    let servers = use_resource(list_language_servers);
    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::Home {}, class: "mcp-back-link", "\u{2190} Back to conversations" }
                h1 { "Language servers" }
                p { class: "muted",
                    "Code intelligence for the model: diagnostics after its edits, and go to definition, references and more. Each server runs in a pod of its own next to a conversation's sandbox, when the model starts it, and sees /workspace."
                }
                Link { to: Route::LanguageServerNewRoute {}, class: "mcp-new-server-link", "+ Add a language server" }
            }
            match servers() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "muted", "None yet. Add one, or look one up in the catalog when adding." }
                },
                Some(Ok(list)) => rsx! {
                    div { class: "mcp-server-list",
                        for server in list {
                            Link {
                                key: "{server.id}",
                                to: Route::LanguageServerEditRoute { id: server.id },
                                class: "mcp-server-row mcp-server-row-link",
                                div { class: "mcp-server-summary",
                                    span { class: "mcp-server-name", "{server.config.name}" }
                                    span { class: "mcp-server-url", "{server.config.image} · {file_types_summary(&server.config)}" }
                                }
                                if server.config.enabled {
                                    span { class: "mcp-status mcp-status-connected", "Enabled" }
                                } else {
                                    span { class: "mcp-status mcp-status-not-connected", "Disabled" }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

fn file_types_summary(config: &LanguageServerConfig) -> String {
    config.file_types.keys().map(|ext| format!(".{ext}")).collect::<Vec<_>>().join(" ")
}

#[component]
pub fn LanguageServerNew() -> Element {
    let navigator = use_navigator();
    let mut error: Signal<Option<String>> = use_signal(|| None);
    // The catalog lookup refills the form: a new `key` remounts it with the
    // suggestion as its starting values.
    let mut initial = use_signal(blank);
    let mut form_key = use_signal(|| 0u32);
    let mut notes: Signal<Vec<String>> = use_signal(Vec::new);
    let mut package = use_signal(String::new);
    let mut looking_up = use_signal(|| false);
    let mut lookup_error: Signal<Option<String>> = use_signal(|| None);
    let look_up = move |event: Event<FormData>| {
        event.prevent_default();
        let name = package().trim().to_string();
        if name.is_empty() {
            return;
        }
        looking_up.set(true);
        lookup_error.set(None);
        spawn(async move {
            match lookup_language_server(name).await {
                Ok(suggestion) => {
                    initial.set(suggestion.config);
                    notes.set(suggestion.notes);
                    *form_key.write() += 1;
                }
                Err(e) => lookup_error.set(Some(super::chat::server_error_message(&e))),
            }
            looking_up.set(false);
        });
    };
    let save = move |config: LanguageServerConfig| {
        spawn(async move {
            match create_language_server(config).await {
                Ok(server) => {
                    navigator.push(Route::LanguageServerEditRoute { id: server.id });
                }
                Err(e) => error.set(Some(super::chat::server_error_message(&e))),
            }
        });
    };
    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::LanguageServersRoute {}, class: "mcp-back-link", "\u{2190} Back to language servers" }
                h1 { "Add a language server" }
            }
            form { class: "mcp-add-form language-server-lookup", onsubmit: look_up,
                label { r#for: "ls-lookup", "Find in the catalog" }
                div { class: "language-server-lookup-row",
                    input { id: "ls-lookup", r#type: "text", placeholder: "rust-analyzer, pyright, gopls...",
                        value: "{package}", oninput: move |e| package.set(e.value()) }
                    button { r#type: "submit", disabled: looking_up(), if looking_up() { "Looking up\u{2026}" } else { "Look up" } }
                }
                p { class: "muted",
                    "A package name from "
                    a { href: "https://mason-registry.dev/registry/list", target: "_blank", rel: "noopener", "mason's registry" }
                    ". smelt fills in the form from it and Helix's language list; check it before adding."
                }
                if let Some(err) = lookup_error() {
                    p { class: "error", "{err}" }
                }
                for note in notes() {
                    p { class: "language-server-note", "{note}" }
                }
            }
            LanguageServerForm { key: "{form_key}", initial: initial(), save_label: "Add language server", error: error(), on_save: save }
        }
    }
}

#[component]
pub fn LanguageServerEdit(id: i64) -> Element {
    let navigator = use_navigator();
    let server = use_resource(move || get_language_server(id));
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut saved: Signal<bool> = use_signal(|| false);
    let mut armed = use_signal(|| false);
    let save = move |config: LanguageServerConfig| {
        spawn(async move {
            match update_language_server(id, config).await {
                Ok(_) => {
                    error.set(None);
                    saved.set(true);
                }
                Err(e) => error.set(Some(super::chat::server_error_message(&e))),
            }
        });
    };
    let delete = move |_| {
        if !armed() {
            armed.set(true);
            return;
        }
        spawn(async move {
            match delete_language_server(id).await {
                Ok(()) => {
                    navigator.push(Route::LanguageServersRoute {});
                }
                Err(e) => error.set(Some(super::chat::server_error_message(&e))),
            }
        });
    };
    rsx! {
        div { class: "mcp-servers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::LanguageServersRoute {}, class: "mcp-back-link", "\u{2190} Back to language servers" }
                h1 { "Edit language server" }
                p { class: "muted", "A server already running keeps these settings until the model restarts it." }
            }
            match server() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { p { class: "error", "{super::chat::server_error_message(&e)}" } },
                Some(Ok(LanguageServer { config, .. })) => rsx! {
                    LanguageServerForm { initial: config, save_label: "Save", error: error(), on_save: save }
                    if saved() {
                        p { class: "muted language-server-saved", "Saved." }
                    }
                    button {
                        class: if armed() { "mcp-delete-server confirm" } else { "mcp-delete-server" },
                        r#type: "button",
                        onclick: delete,
                        super::TwoStepLabel { armed: armed(), idle: "Delete language server", confirm: "Confirm delete?" }
                    }
                },
            }
        }
    }
}

#[component]
fn LanguageServerForm(
    initial: LanguageServerConfig,
    save_label: &'static str,
    error: Option<String>,
    on_save: EventHandler<LanguageServerConfig>,
) -> Element {
    let mut form = use_signal(|| FormText::from_config(&initial));
    let mut form_error: Signal<Option<String>> = use_signal(|| None);
    let submit = move |event: Event<FormData>| {
        event.prevent_default();
        match form().to_config().and_then(|config| config.validate().map(|()| config)) {
            Ok(config) => {
                form_error.set(None);
                on_save.call(config);
            }
            Err(e) => form_error.set(Some(e)),
        }
    };
    let shown_error = form_error().or(error);
    rsx! {
        form { class: "mcp-add-form language-server-form", onsubmit: submit,
            label { r#for: "ls-name", "Name" }
            input { id: "ls-name", r#type: "text", required: true, placeholder: "rust-analyzer",
                value: "{form().name}", oninput: move |e| form.write().name = e.value() }
            p { class: "muted", "Lowercase letters, digits and dashes. The model starts the server by this name." }

            label { r#for: "ls-image", "Image" }
            input { id: "ls-image", r#type: "text", required: true, placeholder: "rust:1",
                value: "{form().image}", oninput: move |e| form.write().image = e.value() }

            label { r#for: "ls-install", "Install command" }
            input { id: "ls-install", r#type: "text", placeholder: "rustup component add rust-analyzer",
                value: "{form().install_command}", oninput: move |e| form.write().install_command = e.value() }
            p { class: "muted", "Runs once when the server's pod starts, as uid 1000 with HOME=/tmp/home. Leave empty if the image already has the server." }

            label { r#for: "ls-command", "Command" }
            input { id: "ls-command", r#type: "text", required: true, placeholder: "rust-analyzer",
                value: "{form().command}", oninput: move |e| form.write().command = e.value() }

            label { r#for: "ls-args", "Arguments, one per line" }
            textarea { id: "ls-args", rows: 2, value: "{form().args}", oninput: move |e| form.write().args = e.value() }

            label { r#for: "ls-file-types", "File types, one per line: extension = language id" }
            textarea { id: "ls-file-types", rows: 3, placeholder: "rs = rust",
                value: "{form().file_types}", oninput: move |e| form.write().file_types = e.value() }

            label { r#for: "ls-roots", "Project root markers, one per line" }
            textarea { id: "ls-roots", rows: 2, placeholder: "Cargo.toml",
                value: "{form().root_markers}", oninput: move |e| form.write().root_markers = e.value() }

            label { r#for: "ls-env", "Environment, one per line: NAME = value" }
            textarea { id: "ls-env", rows: 2, value: "{form().env}", oninput: move |e| form.write().env = e.value() }

            label { r#for: "ls-init", "Initialization options (JSON)" }
            textarea { id: "ls-init", rows: 3, value: "{form().initialization_options}",
                oninput: move |e| form.write().initialization_options = e.value() }

            label { r#for: "ls-settings", "Settings (JSON)" }
            textarea { id: "ls-settings", rows: 3, value: "{form().settings}", oninput: move |e| form.write().settings = e.value() }

            label { r#for: "ls-memory", "Memory limit" }
            input { id: "ls-memory", r#type: "text", required: true, value: "{form().memory_limit}",
                oninput: move |e| form.write().memory_limit = e.value() }
            label { r#for: "ls-cpu", "CPU limit" }
            input { id: "ls-cpu", r#type: "text", required: true, value: "{form().cpu_limit}",
                oninput: move |e| form.write().cpu_limit = e.value() }

            label { class: "language-server-enabled",
                input { r#type: "checkbox", checked: form().enabled, onchange: move |e| form.write().enabled = e.checked() }
                " Enabled (the model can start it)"
            }

            if let Some(err) = shown_error {
                p { class: "error", "{err}" }
            }
            button { r#type: "submit", "{save_label}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_form_round_trips_a_config() {
        let config = LanguageServerConfig {
            name: "rust-analyzer".to_string(),
            image: "rust:1".to_string(),
            install_command: "rustup component add rust-analyzer".to_string(),
            command: "rust-analyzer".to_string(),
            args: vec!["--log-file".to_string(), "/tmp/ra.log".to_string()],
            env: [("RA_LOG".to_string(), "info".to_string())].into(),
            file_types: [("rs".to_string(), "rust".to_string())].into(),
            root_markers: vec!["Cargo.toml".to_string(), "rust-project.json".to_string()],
            initialization_options: Some(serde_json::json!({"cargo": {"targetDir": true}})),
            settings: None,
            memory_limit: "2Gi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
        };
        assert_eq!(FormText::from_config(&config).to_config(), Ok(config));
    }

    #[test]
    fn test_the_form_says_which_field_is_wrong() {
        let form = FormText { file_types: "rs rust".to_string(), ..FormText::default() };
        assert!(form.to_config().expect_err("no =").contains("File types"));
        let form = FormText { settings: "{not json".to_string(), ..FormText::default() };
        assert!(form.to_config().expect_err("bad json").contains("Settings"));
        // A leading dot on an extension is dropped rather than refused.
        let form = FormText { file_types: ".py = python\n\n".to_string(), ..FormText::default() };
        assert_eq!(form.to_config().expect("ok").file_types, [("py".to_string(), "python".to_string())].into());
    }
}
