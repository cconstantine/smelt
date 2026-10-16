use super::*;

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
pub(super) fn thinking_choice(thinking: Option<bool>) -> &'static str {
    match thinking {
        None => "default",
        Some(true) => "on",
        Some(false) => "off",
    }
}

pub(super) fn thinking_from_choice(choice: &str) -> Option<bool> {
    match choice {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// The select's labels; "default" says what it currently means when known.
pub(super) fn thinking_label(choice: &str, provider_thinking: Option<bool>) -> &'static str {
    match (choice, provider_thinking) {
        ("on", _) => "On",
        ("off", _) => "Off",
        (_, Some(true)) => "Provider's default (on)",
        (_, Some(false)) => "Provider's default (off)",
        (_, None) => "Provider's default",
    }
}

/// A model's line in a list: its id, and its display name when it has one.
pub(super) fn model_label(model: &ModelInfo) -> String {
    match &model.display_name {
        Some(name) if name != &model.id => format!("{name} ({})", model.id),
        _ => model.id.clone(),
    }
}

/// A model picker's suggestions, tool-capable ones first: smelt needs
/// tools, so one reported without them goes last.
pub(super) fn suggestion_order(mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models.sort_by_key(|m| m.reported_tools == Some(false));
    models
}

/// Chooses a provider and a model: the provider's listed models are
/// suggestions for a field that also takes an id typed by hand, for when
/// the listing fails or lacks it. Calls `on_choose` with the choice.
#[component]
pub(super) fn ModelChooser(
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
