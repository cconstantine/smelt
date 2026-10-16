use super::*;

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
    /// llama.cpp only (SME-111).
    keep_reasoning: bool,
    /// "Max concurrent requests" as typed (SME-162).
    max_concurrent_requests: String,
}

/// "Max concurrent requests" as sent: blank (or anything but a count) is
/// no limit, `0` the server's own count. The server checks the rest.
pub(super) fn max_requests_from_text(text: &str) -> Option<u32> {
    text.trim().parse().ok()
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
            keep_reasoning: true,
            max_concurrent_requests: String::new(),
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
            keep_reasoning: provider.keep_reasoning,
            max_concurrent_requests: provider.max_concurrent_requests.map(|n| n.to_string()).unwrap_or_default(),
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
            keep_reasoning: self.keep_reasoning,
            max_concurrent_requests: max_requests_from_text(&self.max_concurrent_requests),
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
    /// Suggest the llama.cpp kind when the address answers like one: a new
    /// provider, or one saved as Other (SME-111).
    suggest_kind: bool,
    /// The saved address, asked about when the form opens; empty for a
    /// new provider.
    saved_base_url: String,
    secret_hint: Option<String>,
    save_label: &'static str,
    error: Option<String>,
    on_save: EventHandler<ProviderInput>,
) -> Element {
    let mut form = form;
    let sources = use_resource(|| async { list_price_sources().await.unwrap_or_default() });
    let source_list = move || sources().unwrap_or_default();
    // The address that last answered `/props` as llama.cpp does: the form
    // then suggests the kind, and never switches by itself (SME-111).
    let mut llama_cpp_at: Signal<Option<String>> = use_signal(|| None);
    let probe = move |url: String| {
        if !suggest_kind || url.is_empty() {
            return;
        }
        // The answer is for the address it asked about, if the field
        // still holds it (SME-111 review 1).
        spawn(async move {
            let found = probe_llama_cpp(url.clone()).await.unwrap_or(false);
            let shown = after_probe(llama_cpp_at.peek().clone(), url, found, &form.peek().base_url);
            llama_cpp_at.set(shown);
        });
    };
    // A saved address is asked about once, when the form opens.
    use_hook(move || probe(saved_base_url.trim().to_string()));
    let suggest_llama_cpp = move || {
        let f = form();
        suggest_kind && f.kind != ProviderKind::LlamaCpp && llama_cpp_at().as_deref() == Some(f.base_url.trim())
    };
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
                    ProviderKind::LlamaCpp => "A llama.cpp server (llama-server). smelt reads its /v1/models and /props for the model, its context window and what its chat template supports.",
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
                },
                onchange: move |e| probe(e.value().trim().to_string()) }
            p { class: "muted", "Turns go to this address's /v1/messages." }
            if suggest_llama_cpp() {
                p { class: "provider-kind-suggestion", role: "status",
                    "This looks like a llama.cpp server. "
                    button { r#type: "button", class: "model-picker-button",
                        onclick: move |_| {
                            let mut f = form.write();
                            f.kind = ProviderKind::LlamaCpp;
                            if is_new {
                                f.auth_kind = ProviderKind::LlamaCpp.default_auth_kind();
                                f.prompt_caching = ProviderKind::LlamaCpp.default_prompt_caching();
                            }
                            f.follow_suggestion(&source_list());
                        },
                        "Use the llama.cpp kind"
                    }
                }
            }

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

            if form().kind == ProviderKind::LlamaCpp {
                label { class: "provider-checkbox",
                    input { id: "provider-keep-reasoning", r#type: "checkbox", checked: form().keep_reasoning,
                        onchange: move |e| form.write().keep_reasoning = e.checked() }
                    "Keep earlier reasoning"
                }
                p { class: "muted",
                    "Sends the chat template preserve_thinking, so the model sees its reasoning from earlier turns. Off keeps only the current reply's tool calls' reasoning: conversations stay smaller, and the server re-reads from the first reasoning it drops, once per new message. Only sent when the server's template supports it."
                }
            }

            label { r#for: "provider-max-requests", "Max concurrent requests" }
            input { id: "provider-max-requests", r#type: "number", min: "0", step: "1",
                placeholder: "No limit",
                value: "{form().max_concurrent_requests}",
                oninput: move |e| form.write().max_concurrent_requests = e.value() }
            p { class: "muted",
                "Leave blank for no limit. A server only serves as many requests at once as it has slots. A llama.cpp server can take 0, its own count, which it reports on this provider's page once its models are listed; otherwise type the number. While all slots are busy smelt waits before sending, so two conversations take turns instead of one timing out."
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

            super::super::ErrorText { message: error }
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
            ProviderFields { form, is_new: true, suggest_kind: true, saved_base_url: String::new(), secret_hint: None, save_label: "Add provider", error: error(), on_save: save }
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
                Some(Err(e)) => rsx! { super::super::ErrorText { message: Some(super::super::server_error_message(&e)) } },
                Some(Ok(p)) => rsx! {
                    ProviderFields { form, is_new: false, suggest_kind: p.kind == ProviderKind::Other, saved_base_url: p.base_url.clone(), secret_hint: secret_hint(), save_label: "Save", error: error(), on_save: save }
                    if saved() {
                        p { class: "muted provider-saved", "Saved." }
                    }
                    ProviderModelsSection { id, refresh: models_refresh }
                    super::super::TwoStepButton {
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
