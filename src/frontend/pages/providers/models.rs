use super::*;

/// A provider's models, each with its context window and thinking
/// settings.
#[component]
pub(super) fn ProviderModelsSection(id: i64, refresh: Signal<u64>) -> Element {
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
                Some(Err(e)) => rsx! { super::super::ErrorText { message: Some(super::super::server_error_message(&e)) } },
                Some(Ok(listing)) => rsx! {
                    if listing.kind == ProviderKind::LlamaCpp {
                        LlamaServerLine { caps: listing.server_caps.clone(), error: listing.server_error.clone() }
                    }
                    if let Some(err) = listing.listing_error {
                        p { class: "error", "Couldn't list this provider's models: {err}" }
                    }
                    if listing.models.is_empty() {
                        p { class: "muted", "No models to show." }
                    }
                    div { class: "provider-model-list",
                        for model in listing.models {
                            ProviderModelRow {
                                key: "{model.id}",
                                id,
                                model,
                                kind: listing.kind,
                                efforts: effort_choices(listing.kind, listing.server_caps.as_ref()),
                            }
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

/// The address the llama.cpp suggestion is for, once the probe of `asked`
/// answers `found`: an answer for an address the field no longer holds
/// (a slow probe finishing after a later one) changes nothing.
pub(super) fn after_probe(shown: Option<String>, asked: String, found: bool, current: &str) -> Option<String> {
    if asked != current.trim() {
        return shown;
    }
    found.then_some(asked)
}

/// A token-count field's value: blank for none, commas allowed.
pub(super) fn token_field(text: &str, name: &str) -> Result<Option<u32>, String> {
    let text = text.replace(',', "");
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u32>().map(Some).map_err(|_| format!("{name} is a number of tokens."))
}

/// A llama.cpp server's `/props`, as one line under the models' heading:
/// its build, model, slots and window, and what its template supports.
#[component]
fn LlamaServerLine(caps: Option<LlamaServerInfo>, error: Option<String>) -> Element {
    rsx! {
        if let Some(caps) = caps.clone() {
            p { class: "muted provider-server-info", "{llama_server_summary(&caps)}" }
        }
        if let Some(err) = error {
            p { class: "model-picker-warning provider-server-info",
                if caps.is_some() {
                    "Couldn't read the server's /props just now ({err}); turns use what it said last time."
                } else {
                    "Couldn't read the server's /props ({err}), so turns send its template no settings."
                }
            }
        }
    }
}

/// "llama.cpp b11434 · flash-next · 1 slot · 262,144-token window ·
/// template: preserve reasoning, reasoning effort, tool calls".
pub(super) fn llama_server_summary(caps: &LlamaServerInfo) -> String {
    let mut parts = vec![match &caps.build_info {
        Some(build) => format!("llama.cpp {}", build.split('-').next().unwrap_or(build)),
        None => "llama.cpp".to_string(),
    }];
    if let Some(alias) = &caps.model_alias {
        parts.push(alias.clone());
    }
    if let Some(slots) = caps.total_slots {
        parts.push(if slots == 1 { "1 slot".to_string() } else { format!("{slots} slots") });
    }
    if let Some(window) = caps.n_ctx {
        parts.push(format!("{}-token window", group_digits(window)));
    }
    let supported: Vec<String> = caps
        .template_caps
        .iter()
        .filter(|(_, on)| **on)
        .filter_map(|(name, _)| name.strip_prefix("supports_"))
        .filter(|name| matches!(*name, "reasoning_effort" | "preserve_reasoning" | "tool_calls"))
        .map(|name| name.replace('_', " "))
        .collect();
    parts.push(if supported.is_empty() {
        "template: no reasoning effort or preserved reasoning".to_string()
    } else {
        format!("template: {}", supported.join(", "))
    });
    parts.join(" \u{b7} ")
}

/// The Effort select's levels for a provider's models: every level for
/// Anthropic's and the other kinds' (ignored but by Anthropic, as before);
/// for llama.cpp, low to high when its template reads effort, else none,
/// and the select is hidden (SME-111).
pub(super) fn effort_choices(kind: ProviderKind, caps: Option<&LlamaServerInfo>) -> Vec<crate::anthropic::Effort> {
    match kind {
        ProviderKind::LlamaCpp if caps.is_some_and(LlamaServerInfo::supports_reasoning_effort) => LLAMA_CPP_EFFORTS.to_vec(),
        ProviderKind::LlamaCpp => Vec::new(),
        _ => crate::anthropic::Effort::ALL.to_vec(),
    }
}

#[component]
fn ProviderModelRow(id: i64, model: ModelInfo, kind: ProviderKind, efforts: Vec<crate::anthropic::Effort>) -> Element {
    let llama_cpp_efforts = kind == ProviderKind::LlamaCpp;
    let mut window = use_signal(|| model.context_window_override.map(|w| w.to_string()).unwrap_or_default());
    let mut max_output = use_signal(|| model.max_output_override.map(|n| n.to_string()).unwrap_or_default());
    let mut reasoning = use_signal(|| model.reasoning_budget_override.map(|n| n.to_string()).unwrap_or_default());
    let mut thinking = use_signal(|| model.thinking_override);
    let mut effort = use_signal(|| model.effort_override);
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
    let max_output_placeholder = match model.max_output {
        Some(cap) => group_digits(cap),
        None => "automatic".to_string(),
    };
    let save = move |event: Event<FormData>| {
        event.prevent_default();
        let (context_window, max_output, reasoning_budget) = match (
            token_field(&window(), "The context window"),
            token_field(&max_output(), "Max reply tokens"),
            token_field(&reasoning(), "The reasoning budget"),
        ) {
            (Ok(window), Ok(max_output), Ok(reasoning)) => (window, max_output, reasoning),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                error.set(Some(e));
                return;
            }
        };
        let settings = ModelSettings {
            thinking: thinking(),
            context_window,
            effort: effort(),
            max_output,
            reasoning_budget,
        };
        let model_id = model_id.clone();
        spawn(async move {
            match set_model_settings(id, model_id, settings).await {
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
            label { class: "provider-model-window",
                "Max reply tokens "
                input { r#type: "text", inputmode: "numeric", placeholder: "{max_output_placeholder}", value: "{max_output}",
                    aria_label: "Max reply tokens for {model.id}",
                    title: "The most one reply may be, thinking included. Automatic: what the provider reports, else up to half the context window (16,384 on an Anthropic or other provider that reports none, since a Claude model refuses more than its own cap).",
                    oninput: move |e| {
                        max_output.set(e.value());
                        saved.set(false);
                    } }
            }
            if kind == ProviderKind::LlamaCpp {
                label { class: "provider-model-window",
                    "Reasoning budget "
                    input { r#type: "text", inputmode: "numeric", placeholder: "automatic", value: "{reasoning}",
                        aria_label: "Reasoning budget for {model.id}",
                        title: "The most a reply may spend thinking before llama.cpp makes the model answer. Automatic: three quarters of the reply's budget, leaving at least 4,096 tokens to answer; a number here caps that. At 10 tokens a second, 100,000 tokens is close to three hours.",
                        oninput: move |e| {
                            reasoning.set(e.value());
                            saved.set(false);
                        } }
                }
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
            if !efforts.is_empty() {
                label { class: "provider-model-thinking",
                    "Effort "
                    select {
                        aria_label: "Effort for {model.id}",
                        title: if llama_cpp_efforts {
                            "Sent to the chat template as reasoning_effort. The template decides what each level means; one it refuses fails the turn with the template's error."
                        } else {
                            "Sent as output_config.effort to an Anthropic provider; other kinds ignore it. Changing it re-reads a conversation's cache once."
                        },
                        onchange: move |e| {
                            effort.set(crate::anthropic::Effort::parse(&e.value()));
                            saved.set(false);
                        },
                        option { value: "", selected: effort().is_none(),
                            if llama_cpp_efforts { "template's default" } else { "model's default" }
                        }
                        // Kept when this kind doesn't offer it, so saving
                        // doesn't drop it.
                        if let Some(level) = effort().filter(|level| !efforts.contains(level)) {
                            option { value: "{level.as_str()}", selected: true,
                                if llama_cpp_efforts { "{level.as_str()} (not sent)" } else { "{level.as_str()}" }
                            }
                        }
                        for level in efforts.clone() {
                            option {
                                key: "{level.as_str()}",
                                value: "{level.as_str()}",
                                selected: effort() == Some(level),
                                "{level.as_str()}"
                            }
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
