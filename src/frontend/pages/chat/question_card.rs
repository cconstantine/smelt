//! The card for a question the model asked with `ask_user` (SME-34).

use super::*;

/// The card for the question conversation `conversation_id` waits on
/// (SME-34): each question with its options (buttons, toggles when
/// multi-select) and an "Other" field, then Submit. A single question with
/// single-choice options answers on the click. The card goes when every tab
/// hears the question is answered (`QuestionUpdate { question: None }`);
/// another tab's answer first makes this one's fail with "already answered".
#[component]
pub(super) fn QuestionCard(conversation_id: i64, question: PendingQuestion) -> Element {
    let count = question.questions.len();
    let mut chosen: Signal<Vec<Vec<String>>> = use_signal(|| vec![Vec::new(); count]);
    let mut others: Signal<Vec<String>> = use_signal(|| vec![String::new(); count]);
    let mut error: Signal<Option<String>> = use_signal(|| None);
    let mut sending = use_signal(|| false);
    let answers = move || -> Vec<QuestionAnswer> {
        chosen()
            .into_iter()
            .zip(others())
            .map(|(selected, other)| QuestionAnswer {
                selected,
                other: Some(other.trim().to_string()).filter(|t| !t.is_empty()),
            })
            .collect()
    };
    let complete = move || answers().iter().all(|a| !a.selected.is_empty() || a.other.is_some());
    let tool_use_id = question.tool_use_id.clone();
    let submit = use_callback(move |answers: Vec<QuestionAnswer>| {
        let tool_use_id = tool_use_id.clone();
        sending.set(true);
        error.set(None);
        spawn(async move {
            if let Err(e) = answer_question(conversation_id, tool_use_id, answers).await {
                error.set(Some(server_error_message(&e)));
                sending.set(false);
            }
        });
    });
    let one_click = count == 1 && !question.questions[0].multi_select && !question.questions[0].options.is_empty();
    rsx! {
        div { class: "question-card", role: "group", aria_label: "The model asks",
            for (qi , q) in question.questions.iter().cloned().enumerate() {
                fieldset { key: "{qi}", class: "question-card-question",
                    legend { class: "question-card-header", "{q.header}" }
                    p { class: "question-card-text", "{q.question}" }
                    if !q.options.is_empty() {
                        div { class: "question-card-options",
                            for (oi , option) in q.options.iter().cloned().enumerate() {
                                button {
                                    key: "{oi}",
                                    r#type: "button",
                                    class: if chosen()[qi].contains(&option.label) { "question-option chosen" } else { "question-option" },
                                    aria_pressed: "{chosen()[qi].contains(&option.label)}",
                                    disabled: sending(),
                                    onclick: {
                                        let label = option.label.clone();
                                        let multi = q.multi_select;
                                        move |_| {
                                            {
                                                let mut all = chosen.write();
                                                let picked = &mut all[qi];
                                                if !multi {
                                                    *picked = vec![label.clone()];
                                                } else if let Some(at) = picked.iter().position(|l| *l == label) {
                                                    picked.remove(at);
                                                } else {
                                                    picked.push(label.clone());
                                                }
                                            }
                                            if one_click {
                                                submit(answers());
                                            }
                                        }
                                    },
                                    span { class: "question-option-label", "{option.label}" }
                                    if let Some(description) = option.description.clone() {
                                        span { class: "question-option-description", "{description}" }
                                    }
                                }
                            }
                        }
                    }
                    input {
                        r#type: "text",
                        class: "question-card-other",
                        aria_label: "Your own answer to {q.header}",
                        placeholder: if q.options.is_empty() { "Your answer" } else { "Or write your own answer" },
                        value: "{others()[qi]}",
                        disabled: sending(),
                        oninput: move |e| others.write()[qi] = e.value(),
                    }
                }
            }
            super::ErrorText { message: error() }
            button {
                r#type: "button",
                class: "question-card-submit",
                disabled: sending() || !complete(),
                onclick: move |_| submit(answers()),
                if sending() { "Sending…" } else { "Submit" }
            }
        }
    }
}
