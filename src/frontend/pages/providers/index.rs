use super::*;

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
                    super::super::ErrorText { message: default_error() }
                }
            }

            match providers() {
                None => rsx! { p { class: "muted", "Loading..." } },
                Some(Err(e)) => rsx! { super::super::ErrorText { message: Some(super::super::server_error_message(&e)) } },
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
                                            "{crate::models::calls_text(row.calls)} \u{b7} input {row.input_tokens} uncached, {row.cache_creation_input_tokens} written, "
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
pub(super) fn model_spend_label(row: &crate::models::ModelSpend) -> String {
    match &row.provider_name {
        Some(provider) => format!("{provider} \u{b7} {}", row.model),
        None => format!("{} (deleted provider)", row.model),
    }
}

#[cfg(test)]
mod spend_label_tests {
    use super::super::*;

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
