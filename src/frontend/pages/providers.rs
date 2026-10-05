//! The model providers pages (`/providers`, SME-72) and the model picker
//! above a conversation's message box.

use dioxus::prelude::*;

use crate::api::chat::{get_conversation_model, set_conversation_model};
use crate::api::providers::{
    add_provider_model, create_provider, delete_provider, get_default_model, get_provider, get_recent_spend, list_price_sources,
    list_provider_models, list_providers, refresh_model_details, set_default_model, set_model_settings, update_provider,
};
use crate::frontend::Route;
use crate::providers::{
    suggest_price_source, AuthKind, ConversationModel, ModelChoice, ModelInfo, PriceSource, ProviderInput, ProviderKind,
    ProviderSummary, ASSUMED_CONTEXT_WINDOW,
};

use super::server_error_message;

/// "Provider · model", as the picker and the default show a choice.
pub(crate) fn choice_label(choice: &ModelChoice) -> String {
    format!("{} \u{b7} {}", choice.provider_name, choice.model)
}

/// What to warn about a chosen model, if anything: tools it can't call
/// (smelt needs them), or a context window nothing sized.
pub(crate) fn choice_warnings(choice: &ModelChoice) -> Vec<String> {
    let mut warnings = Vec::new();
    if choice.tools == Some(false) {
        warnings.push("The provider says this model can't call tools, which smelt needs.".to_string());
    }
    if !choice.context_window_known {
        warnings.push(format!(
            "Context window unknown, assuming {}.",
            group_digits(ASSUMED_CONTEXT_WINDOW)
        ));
    }
    warnings
}

/// `200000` as `200,000`.
pub(crate) fn group_digits(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A thinking override as the row's select names it.
fn thinking_choice(thinking: Option<bool>) -> &'static str {
    match thinking {
        None => "default",
        Some(true) => "on",
        Some(false) => "off",
    }
}

fn thinking_from_choice(choice: &str) -> Option<bool> {
    match choice {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// The select's labels; "default" says what it currently means when known.
fn thinking_label(choice: &str, provider_thinking: Option<bool>) -> &'static str {
    match (choice, provider_thinking) {
        ("on", _) => "On",
        ("off", _) => "Off",
        (_, Some(true)) => "Provider's default (on)",
        (_, Some(false)) => "Provider's default (off)",
        (_, None) => "Provider's default",
    }
}

/// A model's line in a list: its id, and its display name when it has one.
fn model_label(model: &ModelInfo) -> String {
    match &model.display_name {
        Some(name) if name != &model.id => format!("{name} ({})", model.id),
        _ => model.id.clone(),
    }
}

/// A model picker's suggestions, tool-capable ones first: smelt needs
/// tools, so one reported without them goes last.
fn suggestion_order(mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models.sort_by_key(|m| m.reported_tools == Some(false));
    models
}

/// Chooses a provider and a model: the provider's listed models are
/// suggestions for a field that also takes an id typed by hand, for when
/// the listing fails or lacks it. Calls `on_choose` with the choice.
#[component]
fn ModelChooser(
    providers: Vec<ProviderSummary>,
    initial_provider: Option<i64>,
    initial_model: String,
    choose_label: &'static str,
    on_choose: EventHandler<(i64, String)>,
    on_cancel: Option<EventHandler<()>>,
) -> Element {
    let first = initial_provider.or_else(|| providers.first().map(|p| p.id));
    let mut provider = use_signal(|| first);
    let mut model = use_signal(|| initial_model.clone());
    let listing = use_resource(move || async move {
        match provider() {
            Some(id) => Some(list_provider_models(id, false).await),
            None => None,
        }
    });
    let list_id = format!("model-suggestions-{}", provider().unwrap_or_default());
    let choose = move |event: Event<FormData>| {
        event.prevent_default();
        let (Some(id), chosen) = (provider(), model().trim().to_string()) else {
            return;
        };
        if !chosen.is_empty() {
            on_choose.call((id, chosen));
        }
    };
    rsx! {
        form { class: "model-chooser", onsubmit: choose,
            select {
                class: "model-chooser-provider",
                aria_label: "Provider",
                onchange: move |e| {
                    provider.set(e.value().parse().ok());
                    model.set(String::new());
                },
                for p in providers.iter() {
                    option { key: "{p.id}", value: "{p.id}", selected: provider() == Some(p.id), "{p.name}" }
                }
            }
            input {
                class: "model-chooser-model",
                r#type: "text",
                aria_label: "Model",
                list: "{list_id}",
                placeholder: "Model id",
                autocomplete: "off",
                spellcheck: "false",
                value: "{model}",
                oninput: move |e| model.set(e.value()),
            }
            datalist { id: "{list_id}",
                if let Some(Some(Ok(listing))) = listing() {
                    for m in suggestion_order(listing.models) {
                        option {
                            key: "{m.id}",
                            value: "{m.id}",
                            if m.reported_tools == Some(false) { "{model_label(&m)} \u{2014} no tool support" } else { "{model_label(&m)}" }
                        }
                    }
                }
            }
            button { r#type: "submit", disabled: model().trim().is_empty() || provider().is_none(), "{choose_label}" }
            if let Some(cancel) = on_cancel {
                button { r#type: "button", class: "model-chooser-cancel", onclick: move |_| cancel.call(()), "Cancel" }
            }
            match listing() {
                None | Some(None) => rsx! {},
                Some(Some(Ok(listing))) => rsx! {
                    if let Some(error) = listing.listing_error {
                        p { class: "model-chooser-note muted", "Couldn't list this provider's models ({error}). Type a model id." }
                    } else if listing.models.is_empty() {
                        p { class: "model-chooser-note muted", "This provider lists no models. Type a model id." }
                    }
                },
                Some(Some(Err(e))) => rsx! {
                    p { class: "model-chooser-note error", "{server_error_message(&e)}" }
                },
            }
        }
    }
}

/// Which model a conversation's next turn uses, above its message box,
/// with a way to change it. Sets `ready` to whether there's a model to
/// send to. Refetches when `refresh` changes (a `ModelChanged` or
/// `ProvidersChanged` event). Keyed by conversation where it's used.
#[component]
pub(crate) fn ModelPicker(conversation_id: i64, refresh: Signal<u64>, ready: Signal<bool>) -> Element {
    let mut local_refresh = use_signal(|| 0u64);
    let state = use_resource(move || async move {
        refresh();
        local_refresh();
        get_conversation_model(conversation_id).await
    });
    let providers = use_resource(move || async move {
        refresh();
        list_providers().await
    });
    let mut choosing = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut ready = ready;
    // Ready unless the picker knows there's no model: while it loads, a
    // send goes ahead and the server says so if there's none.
    use_effect(move || {
        let not_ready = matches!(&*state.read(), Some(Ok(model)) if !model.is_ready());
        ready.set(!not_ready);
    });
    let choose = move |(provider_id, model): (i64, String)| {
        spawn(async move {
            match set_conversation_model(conversation_id, provider_id, model.clone()).await {
                Ok(()) => {
                    // Ask the provider about it now, so its window and
                    // tools are known before the next turn. This task
                    // runs in the chooser's scope, so the chooser closes
                    // only after the last await: closing it cancels us.
                    let refreshed = refresh_model_details(provider_id, model).await;
                    error.set(refreshed.err().map(|e| server_error_message(&e)));
                    *local_refresh.write() += 1;
                    choosing.set(false);
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    let (provider_list, providers_error) = match providers() {
        Some(Ok(list)) => (list, None),
        Some(Err(e)) => (Vec::new(), Some(server_error_message(&e))),
        None => (Vec::new(), None),
    };
    let (current_provider, current_model) = match state() {
        Some(Ok(model)) => model
            .choice()
            .map_or((None, String::new()), |c| (Some(c.provider_id), c.model.clone())),
        _ => (None, String::new()),
    };
    rsx! {
        div { class: "model-picker",
            match state() {
                None => rsx! { span { class: "muted", "Model\u{2026}" } },
                Some(Err(e)) => rsx! { span { class: "error", "{server_error_message(&e)}" } },
                Some(Ok(ConversationModel::NoProviders {})) => rsx! {
                    p { class: "model-picker-setup", role: "status",
                        "No model provider is set up, so there's nothing to send to. "
                        Link { to: Route::ProvidersRoute {}, "Add a model provider" }
                    }
                },
                Some(Ok(ConversationModel::NoDefault {})) if !choosing() => rsx! {
                    p { class: "model-picker-setup", role: "status",
                        "No model is chosen for this conversation. "
                        button { r#type: "button", class: "model-picker-button", onclick: move |_| choosing.set(true), "Choose a model" }
                    }
                },
                Some(Ok(ConversationModel::Chosen { choice })) if !choosing() => rsx! {
                    ModelPickerCurrent { choice, is_default: false, on_change: move |_| choosing.set(true) }
                },
                Some(Ok(ConversationModel::Default { choice })) if !choosing() => rsx! {
                    ModelPickerCurrent { choice, is_default: true, on_change: move |_| choosing.set(true) }
                },
                Some(Ok(_)) if provider_list.is_empty() => rsx! {
                    match providers_error.clone() {
                        Some(err) => rsx! { span { class: "error", "Couldn't load the providers: {err}" } },
                        None => rsx! { span { class: "muted", "Loading providers\u{2026}" } },
                    }
                },
                Some(Ok(_)) => rsx! {
                    ModelChooser {
                        providers: provider_list,
                        initial_provider: current_provider,
                        initial_model: current_model,
                        choose_label: "Use",
                        on_choose: choose,
                        on_cancel: move |_| choosing.set(false),
                    }
                },
            }
            if let Some(err) = error() {
                p { class: "error model-picker-error", "{err}" }
            }
        }
    }
}

#[component]
fn ModelPickerCurrent(choice: ModelChoice, is_default: bool, on_change: EventHandler<()>) -> Element {
    let label = choice_label(&choice);
    let warnings = choice_warnings(&choice);
    rsx! {
        div { class: "model-picker-current",
            span { class: "model-picker-label", title: "The model this conversation's next turn runs on",
                if is_default { "Default: {label}" } else { "{label}" }
            }
            button { r#type: "button", class: "model-picker-button", onclick: move |_| on_change.call(()), "Change" }
            for warning in warnings {
                span { class: "model-picker-warning",
                    "{warning} "
                    Link { to: Route::ProviderEditRoute { id: choice.provider_id }, "Model settings" }
                }
            }
        }
    }
}

#[component]
pub fn ProvidersIndex() -> Element {
    let mut refresh = use_signal(|| 0u64);
    let providers = use_resource(move || async move {
        refresh();
        list_providers().await
    });
    let default = use_resource(move || async move {
        refresh();
        get_default_model().await
    });
    let recent_spend = use_resource(move || async move { get_recent_spend().await });
    let mut changing_default = use_signal(|| false);
    let mut default_error: Signal<Option<String>> = use_signal(|| None);
    let choose_default = move |(provider_id, model): (i64, String)| {
        spawn(async move {
            match set_default_model(provider_id, model.clone()).await {
                Ok(()) => {
                    // Closing the chooser cancels this task (it runs in the
                    // chooser's scope), so that comes last.
                    let refreshed = refresh_model_details(provider_id, model).await;
                    default_error.set(refreshed.err().map(|e| server_error_message(&e)));
                    *refresh.write() += 1;
                    changing_default.set(false);
                }
                Err(e) => default_error.set(Some(server_error_message(&e))),
            }
        });
    };
    let provider_list = match providers() {
        Some(Ok(list)) => list,
        _ => Vec::new(),
    };
    let current_default = match default() {
        Some(Ok(choice)) => choice,
        _ => None,
    };
    rsx! {
        div { class: "mcp-servers-page providers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::Home {}, class: "mcp-back-link", "\u{2190} Back to conversations" }
                h1 { "Model providers" }
                p { class: "muted",
                    "Where the model runs: Anthropic, an Ollama server, or any other server with an Anthropic-compatible /v1/messages. Each conversation can use a different one."
                }
                Link { to: Route::ProviderNewRoute {}, class: "mcp-new-server-link", "+ Add a provider" }
            }

            if !provider_list.is_empty() {
                section { class: "providers-default",
                    h2 { "Default model" }
                    p { class: "muted", "A conversation without a model of its own takes this one when its next turn starts, and keeps it." }
                    if changing_default() || current_default.is_none() {
                        ModelChooser {
                            providers: provider_list.clone(),
                            initial_provider: current_default.as_ref().map(|c| c.provider_id),
                            initial_model: current_default.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
                            choose_label: "Make default",
                            on_choose: choose_default,
                            on_cancel: current_default.is_some().then_some(EventHandler::new(move |_| changing_default.set(false))),
                        }
                    } else if let Some(choice) = current_default.clone() {
                        div { class: "model-picker-current",
                            span { class: "model-picker-label providers-default-label", "{choice_label(&choice)}" }
                            button { r#type: "button", class: "model-picker-button", onclick: move |_| changing_default.set(true), "Change" }
                            for warning in choice_warnings(&choice) {
                                span { class: "model-picker-warning", "{warning}" }
                            }
                        }
                    }
                    super::ErrorText { message: default_error() }
                }
            }

            match providers() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { super::ErrorText { message: Some(super::server_error_message(&e)) } },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "muted providers-empty", "No providers yet. Add one to start chatting." }
                },
                Some(Ok(list)) => rsx! {
                    div { class: "mcp-server-list",
                        for provider in list {
                            Link {
                                key: "{provider.id}",
                                to: Route::ProviderEditRoute { id: provider.id },
                                class: "mcp-server-row mcp-server-row-link",
                                div { class: "mcp-server-summary",
                                    span { class: "mcp-server-name", "{provider.name}" }
                                    span { class: "mcp-server-url", "{provider.kind.label()} \u{b7} {provider.base_url}" }
                                }
                                span { class: "mcp-status mcp-status-not-connected provider-key-hint",
                                    match &provider.secret_hint {
                                        Some(hint) => format!("Key {hint}"),
                                        None => "Key set".to_string(),
                                    }
                                }
                            }
                        }
                    }
                },
            }

            if let Some(Ok(spend)) = recent_spend() {
                if !spend.is_empty() {
                    section { class: "providers-spend",
                        h2 { "Last 30 days" }
                        p { class: "muted",
                            "Completed model calls, priced from each provider's \u{201c}Prices from\u{201d} entry when they finished."
                        }
                        div { class: "mcp-server-list",
                            for row in spend {
                                div { class: "mcp-server-row",
                                    key: "{row.provider_name.clone().unwrap_or_default()}/{row.model}",
                                    div { class: "mcp-server-summary",
                                        span { class: "mcp-server-name", "{model_spend_label(&row)}" }
                                        span { class: "mcp-server-url",
                                            "{row.calls} calls \u{b7} input {row.input_tokens} uncached, {row.cache_creation_input_tokens} written, "
                                            "{row.cache_read_input_tokens} read \u{b7} output {row.output_tokens}"
                                        }
                                    }
                                    span { class: "providers-spend-cost",
                                        "{crate::models::cost_text(row.calls, row.cost_usd, row.unpriced_calls)}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// "Provider · model" for a spend row, or the model alone under a deleted
/// provider.
fn model_spend_label(row: &crate::models::ModelSpend) -> String {
    match &row.provider_name {
        Some(provider) => format!("{provider} \u{b7} {}", row.model),
        None => format!("{} (deleted provider)", row.model),
    }
}

#[cfg(test)]
mod spend_label_tests {
    use super::*;

    #[test]
    fn test_a_spend_row_names_its_provider_or_says_it_was_deleted() {
        let row = |provider: Option<&str>| crate::models::ModelSpend {
            provider_name: provider.map(str::to_string),
            model: "m".to_string(),
            ..Default::default()
        };
        assert_eq!(model_spend_label(&row(Some("Anthropic"))), "Anthropic \u{b7} m");
        assert_eq!(model_spend_label(&row(None)), "m (deleted provider)");
    }
}

/// The provider form's fields as text.
#[derive(Clone, Debug, PartialEq)]
struct ProviderForm {
    name: String,
    kind: ProviderKind,
    base_url: String,
    auth_kind: AuthKind,
    secret: String,
    prompt_caching: bool,
    /// The catalog provider that prices its calls (SME-106).
    price_source: Option<String>,
    /// Picked by the user: no longer follows the address.
    price_source_chosen: bool,
}

impl ProviderForm {
    fn new_for(kind: ProviderKind) -> Self {
        ProviderForm {
            name: String::new(),
            kind,
            base_url: String::new(),
            auth_kind: kind.default_auth_kind(),
            secret: String::new(),
            prompt_caching: kind.default_prompt_caching(),
            price_source: None,
            price_source_chosen: false,
        }
    }

    fn from_summary(provider: &ProviderSummary) -> Self {
        ProviderForm {
            name: provider.name.clone(),
            kind: provider.kind,
            base_url: provider.base_url.clone(),
            auth_kind: provider.auth_kind,
            secret: String::new(),
            prompt_caching: provider.prompt_caching,
            price_source: provider.price_catalog_provider.clone(),
            price_source_chosen: true,
        }
    }

    fn input(&self) -> ProviderInput {
        ProviderInput {
            name: self.name.clone(),
            kind: self.kind,
            base_url: self.base_url.clone(),
            auth_kind: self.auth_kind,
            secret: self.secret.clone(),
            prompt_caching: self.prompt_caching,
            price_catalog_provider: self.price_source.clone(),
        }
    }

    /// Follows the address and kind with the suggested price source, until
    /// the user picks one.
    fn follow_suggestion(&mut self, sources: &[PriceSource]) {
        if !self.price_source_chosen {
            self.price_source = suggest_price_source(sources, self.kind, &self.base_url);
        }
    }
}

#[component]
fn ProviderFields(
    form: Signal<ProviderForm>,
    is_new: bool,
    secret_hint: Option<String>,
    save_label: &'static str,
    error: Option<String>,
    on_save: EventHandler<ProviderInput>,
) -> Element {
    let mut form = form;
    let sources = use_resource(|| async { list_price_sources().await.unwrap_or_default() });
    let source_list = move || sources().unwrap_or_default();
    let submit = move |event: Event<FormData>| {
        event.prevent_default();
        on_save.call(form().input());
    };
    let secret_placeholder = match (is_new, &secret_hint) {
        (true, _) => "The API key or token".to_string(),
        (false, Some(hint)) => format!("Stored ({hint}). Leave blank to keep it, unless the URL changes."),
        (false, None) => "Stored. Leave blank to keep it, unless the URL changes.".to_string(),
    };
    rsx! {
        form { class: "mcp-add-form provider-form", onsubmit: submit,
            label { r#for: "provider-kind", "Kind" }
            select {
                id: "provider-kind",
                onchange: move |e| {
                    if let Some(kind) = ProviderKind::parse(&e.value()) {
                        let mut f = form.write();
                        f.kind = kind;
                        if is_new {
                            f.auth_kind = kind.default_auth_kind();
                            f.prompt_caching = kind.default_prompt_caching();
                        }
                        f.follow_suggestion(&source_list());
                    }
                },
                for kind in ProviderKind::ALL {
                    option { key: "{kind.as_str()}", value: "{kind.as_str()}", selected: form().kind == kind, "{kind.label()}" }
                }
            }
            p { class: "muted",
                match form().kind {
                    ProviderKind::Anthropic => "Anthropic's API. Its model list says each model's context window.",
                    ProviderKind::Ollama => "An Ollama server. smelt asks it which models it has, what each can do, and each model's context window where it can tell.",
                    ProviderKind::Other => "Any other server with an Anthropic-compatible /v1/messages, such as a gateway or llama.cpp. smelt tries its /v1/models for the list.",
                }
            }

            label { r#for: "provider-name", "Name" }
            input { id: "provider-name", r#type: "text", required: true, placeholder: "Anthropic",
                value: "{form().name}", oninput: move |e| form.write().name = e.value() }

            label { r#for: "provider-url", "Base URL" }
            input { id: "provider-url", r#type: "url", required: true, placeholder: "{form().kind.base_url_placeholder()}",
                value: "{form().base_url}", oninput: move |e| {
                    let mut f = form.write();
                    f.base_url = e.value();
                    f.follow_suggestion(&source_list());
                } }
            p { class: "muted", "Turns go to this address's /v1/messages." }

            label { r#for: "provider-auth", "Sent as" }
            select {
                id: "provider-auth",
                onchange: move |e| {
                    if let Some(auth) = AuthKind::parse(&e.value()) {
                        form.write().auth_kind = auth;
                    }
                },
                for auth in AuthKind::ALL {
                    option { key: "{auth.as_str()}", value: "{auth.as_str()}", selected: form().auth_kind == auth, "{auth.label()}" }
                }
            }

            label { r#for: "provider-secret", "API key or token" }
            input { id: "provider-secret", r#type: "password", required: is_new, autocomplete: "off",
                placeholder: "{secret_placeholder}",
                value: "{form().secret}", oninput: move |e| form.write().secret = e.value() }
            p { class: "muted",
                "Stays on the server: pages only ever show its last four characters. A local Ollama needs one but ignores it, so any value does."
            }

            label { class: "provider-checkbox",
                input { id: "provider-caching", r#type: "checkbox", checked: form().prompt_caching,
                    onchange: move |e| form.write().prompt_caching = e.checked() }
                "Prompt caching"
            }
            p { class: "muted",
                "Marks each turn's request so the server can reuse the conversation so far instead of reading it again; on Anthropic a cached read costs a tenth of the input price. Turn it off if this server refuses requests with a cache_control error."
            }

            label { r#for: "provider-prices", "Prices from" }
            select {
                id: "provider-prices",
                onchange: move |e| {
                    let mut f = form.write();
                    f.price_source = Some(e.value()).filter(|v| !v.is_empty());
                    f.price_source_chosen = true;
                },
                option { value: "", selected: form().price_source.is_none(), "None: show tokens only" }
                // Kept when the list doesn't have it (not fetched yet, or
                // gone from the catalog), so saving doesn't drop it.
                if let Some(id) = form().price_source.filter(|id| !source_list().iter().any(|s| &s.id == id)) {
                    option { value: "{id}", selected: true, "{id}" }
                }
                for source in source_list() {
                    option { key: "{source.id}", value: "{source.id}",
                        selected: form().price_source.as_deref() == Some(source.id.as_str()),
                        "{source.name} ({source.id})" }
                }
            }
            p { class: "muted",
                "Each call's cost comes from this entry in the models.dev price list, refreshed hourly. Pick the plan's own entry for a flat-rate coding plan (its calls cost $0), or None for a local server."
            }
            if form().price_source.is_none() {
                if let Some(suggested) = suggest_price_source(&source_list(), form().kind, &form().base_url) {
                    p { class: "muted", "models.dev lists this address as \u{201c}{suggested}\u{201d}." }
                }
            }

            super::ErrorText { message: error }
            button { r#type: "submit", "{save_label}" }
        }
    }
}

#[component]
pub fn ProviderNew() -> Element {
    let navigator = use_navigator();
    let form = use_signal(|| ProviderForm::new_for(ProviderKind::Anthropic));
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let save = move |input: ProviderInput| {
        spawn(async move {
            match create_provider(input).await {
                Ok(provider) => {
                    navigator.push(Route::ProviderEditRoute { id: provider.id });
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    rsx! {
        div { class: "mcp-servers-page providers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::ProvidersRoute {}, class: "mcp-back-link", "\u{2190} Back to model providers" }
                h1 { "Add a model provider" }
            }
            ProviderFields { form, is_new: true, secret_hint: None, save_label: "Add provider", error: error(), on_save: save }
        }
    }
}

#[component]
pub fn ProviderEdit(id: i64) -> Element {
    let navigator = use_navigator();
    let provider = use_resource(move || get_provider(id));
    let mut form = use_signal(|| ProviderForm::new_for(ProviderKind::Other));
    let mut loaded = use_signal(|| false);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut saved = use_signal(|| false);
    let mut armed = use_signal(|| false);
    let mut models_refresh = use_signal(|| 0u64);
    // The key's hint, updated by a save that sets a new key.
    let mut secret_hint: Signal<Option<String>> = use_signal(|| None);
    use_effect(move || {
        if let Some(Ok(p)) = &*provider.read()
            && !loaded()
        {
            form.set(ProviderForm::from_summary(p));
            secret_hint.set(p.secret_hint.clone());
            loaded.set(true);
        }
    });
    let save = move |input: ProviderInput| {
        spawn(async move {
            match update_provider(id, input).await {
                Ok(saved_provider) => {
                    secret_hint.set(saved_provider.secret_hint);
                    error.set(None);
                    saved.set(true);
                    form.write().secret.clear();
                    *models_refresh.write() += 1;
                }
                Err(e) => {
                    saved.set(false);
                    error.set(Some(server_error_message(&e)));
                }
            }
        });
    };
    let delete = move |_| {
        spawn(async move {
            match delete_provider(id).await {
                Ok(()) => {
                    navigator.push(Route::ProvidersRoute {});
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    rsx! {
        div { class: "mcp-servers-page providers-page",
            div { class: "mcp-servers-header",
                Link { to: Route::ProvidersRoute {}, class: "mcp-back-link", "\u{2190} Back to model providers" }
                h1 { "Edit model provider" }
                p { class: "muted", "A turn already running keeps the settings it started with." }
            }
            match provider() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { super::ErrorText { message: Some(super::server_error_message(&e)) } },
                Some(Ok(_)) => rsx! {
                    ProviderFields { form, is_new: false, secret_hint: secret_hint(), save_label: "Save", error: error(), on_save: save }
                    if saved() {
                        p { class: "muted provider-saved", "Saved." }
                    }
                    ProviderModelsSection { id, refresh: models_refresh }
                    super::TwoStepButton {
                        armed: armed(),
                        class: "mcp-delete-server",
                        idle: "Delete provider",
                        confirm: "Confirm delete? Its conversations take the default at their next turn, if there's still one",
                        on_arm: move |_| armed.set(true),
                        on_confirm: delete,
                    }
                },
            }
        }
    }
}

/// A provider's models, each with its context window and thinking
/// settings.
#[component]
fn ProviderModelsSection(id: i64, refresh: Signal<u64>) -> Element {
    let mut local_refresh = use_signal(|| 0u64);
    let listing = use_resource(move || async move {
        refresh();
        local_refresh();
        list_provider_models(id, true).await
    });
    let mut typed = use_signal(String::new);
    let mut add_error: Signal<Option<String>> = use_signal(|| None);
    rsx! {
        section { class: "provider-models",
            h2 { "Models" }
            p { class: "muted",
                "What smelt knows about each model. Set a context window to override what the provider reports; smelt compacts a conversation before it outgrows it."
            }
            match listing() {
                None => rsx! { p { class: "muted", "Asking the provider\u{2026}" } },
                Some(Err(e)) => rsx! { super::ErrorText { message: Some(super::server_error_message(&e)) } },
                Some(Ok(listing)) => rsx! {
                    if let Some(err) = listing.listing_error {
                        p { class: "error", "Couldn't list this provider's models: {err}" }
                    }
                    if listing.models.is_empty() {
                        p { class: "muted", "No models to show." }
                    }
                    div { class: "provider-model-list",
                        for model in listing.models {
                            ProviderModelRow { key: "{model.id}", id, model }
                        }
                    }
                },
            }
            form {
                class: "provider-model-add",
                onsubmit: move |event: Event<FormData>| {
                    event.prevent_default();
                    let model = typed().trim().to_string();
                    if model.is_empty() {
                        return;
                    }
                    spawn(async move {
                        match add_provider_model(id, model).await {
                            Ok(()) => {
                                add_error.set(None);
                                typed.set(String::new());
                                *local_refresh.write() += 1;
                            }
                            Err(e) => add_error.set(Some(server_error_message(&e))),
                        }
                    });
                },
                label { r#for: "provider-model-typed", "A model the list doesn't show" }
                input { id: "provider-model-typed", r#type: "text", placeholder: "Model id", value: "{typed}",
                    oninput: move |e| typed.set(e.value()) }
                button { r#type: "submit", "Add" }
                if let Some(err) = add_error() {
                    span { class: "error", "{err}" }
                }
            }
        }
    }
}

#[component]
fn ProviderModelRow(id: i64, model: ModelInfo) -> Element {
    let mut window = use_signal(|| model.context_window_override.map(|w| w.to_string()).unwrap_or_default());
    let mut thinking = use_signal(|| model.thinking_override);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut saved = use_signal(|| false);
    let model_id = model.id.clone();
    // What "provider's default" currently means: shown only when the row
    // has no override, since `model.thinking` is then the provider's.
    let provider_thinking = if model.thinking_override.is_some() {
        None
    } else {
        Some(model.thinking)
    };
    let window_placeholder = if model.context_window_known {
        group_digits(model.context_window)
    } else {
        format!("unknown, assuming {}", group_digits(ASSUMED_CONTEXT_WINDOW))
    };
    let save = move |event: Event<FormData>| {
        event.prevent_default();
        let text = window().replace(',', "");
        let text = text.trim();
        let context_window = if text.is_empty() {
            None
        } else {
            match text.parse::<u32>() {
                Ok(n) => Some(n),
                Err(_) => {
                    error.set(Some("The context window is a number of tokens.".to_string()));
                    return;
                }
            }
        };
        let model_id = model_id.clone();
        spawn(async move {
            match set_model_settings(id, model_id, thinking(), context_window).await {
                Ok(()) => {
                    error.set(None);
                    saved.set(true);
                }
                Err(e) => error.set(Some(server_error_message(&e))),
            }
        });
    };
    rsx! {
        form { class: "provider-model-row", onsubmit: save,
            span { class: "provider-model-name", "{model_label(&model)}" }
            if model.reported_tools == Some(false) {
                span { class: "model-picker-warning", "No tool support" }
            }
            label { class: "provider-model-window",
                "Context window "
                input { r#type: "text", inputmode: "numeric", placeholder: "{window_placeholder}", value: "{window}",
                    aria_label: "Context window for {model.id}",
                    oninput: move |e| {
                        window.set(e.value());
                        saved.set(false);
                    } }
            }
            label { class: "provider-model-thinking",
                "Thinking "
                select {
                    aria_label: "Thinking for {model.id}",
                    onchange: move |e| {
                        thinking.set(thinking_from_choice(&e.value()));
                        saved.set(false);
                    },
                    for choice in ["default", "on", "off"] {
                        option {
                            key: "{choice}",
                            value: "{choice}",
                            selected: thinking_choice(thinking()) == choice,
                            "{thinking_label(choice, provider_thinking)}"
                        }
                    }
                }
            }
            button { r#type: "submit", "Save" }
            if saved() {
                span { class: "muted", "Saved." }
            }
            if let Some(err) = model.details_error.clone() {
                span { class: "model-picker-warning", "Couldn't ask about this model: {err}" }
            }
            if let Some(err) = error() {
                span { class: "error", "{err}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(tools: Option<bool>, known: bool) -> ModelChoice {
        ModelChoice {
            provider_id: 1,
            provider_name: "Home".to_string(),
            model: "gemma4".to_string(),
            context_window: 4096,
            context_window_known: known,
            tools,
        }
    }

    #[test]
    fn test_a_choice_warns_about_missing_tools_and_an_unknown_window() {
        assert_eq!(choice_label(&choice(None, true)), "Home \u{b7} gemma4");
        assert!(choice_warnings(&choice(None, true)).is_empty());
        assert!(choice_warnings(&choice(Some(true), true)).is_empty());
        let warnings = choice_warnings(&choice(Some(false), false));
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("tools"), "{warnings:?}");
        assert!(warnings[1].contains("200,000"), "{warnings:?}");
    }

    /// SME-72 review: "provider's default" is a choice of its own, so
    /// saving a context window doesn't pin thinking.
    #[test]
    fn test_thinking_choices_round_trip_including_no_override() {
        for thinking in [None, Some(true), Some(false)] {
            assert_eq!(thinking_from_choice(thinking_choice(thinking)), thinking);
        }
        assert_eq!(thinking_label("default", Some(false)), "Provider's default (off)");
    }

    #[test]
    fn test_digits_are_grouped_by_thousands() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(4096), "4,096");
        assert_eq!(group_digits(200_000), "200,000");
        assert_eq!(group_digits(1_048_576), "1,048,576");
    }

    #[test]
    fn test_models_without_tools_are_suggested_last() {
        let model = |id: &str, tools: Option<bool>| ModelInfo {
            id: id.to_string(),
            display_name: None,
            thinking_override: None,
            context_window_override: None,
            reported_context_window: None,
            reported_tools: tools,
            thinking: true,
            context_window: 1,
            context_window_known: true,
            details_error: None,
        };
        let ordered = suggestion_order(vec![model("a", Some(false)), model("b", None), model("c", Some(true))]);
        let ids: Vec<&str> = ordered.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }
}
