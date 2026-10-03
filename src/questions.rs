//! The model's `ask_user` tool (SME-34): one to four questions the model
//! asks the user, answered on a card in the transcript. The turn ends while
//! the question waits; the answer becomes the call's `tool_result` and a
//! new turn starts on it.
//!
//! The wire types are ungated: the card's snapshot and its answer cross the
//! client/server boundary. Parsing the model's input, checking an answer
//! and wording the result are server-only (`server`).

use serde::{Deserialize, Serialize};

/// The tool's name, as the model calls it and the page recognises it.
pub const ASK_USER: &str = "ask_user";

/// One choice the model offers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question: free text only when it has no options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub question: String,
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// The question a conversation is waiting on: the card's snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingQuestion {
    pub tool_use_id: String,
    pub questions: Vec<Question>,
}

/// The user's answer to one question: the option labels they chose and
/// anything they wrote in the "Other" field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QuestionAnswer {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

#[cfg(feature = "server")]
mod server {
    use super::*;

    /// The most questions one call may ask.
    pub const MAX_QUESTIONS: usize = 4;
    /// The longest header, in characters.
    pub const MAX_HEADER_CHARS: usize = 30;
    /// The fewest and most options a question with options may offer.
    pub const MIN_OPTIONS: usize = 2;
    pub const MAX_OPTIONS: usize = 4;

    /// The call's result when the user wrote a message instead of
    /// answering; their message follows it in the same user message.
    pub const NOT_ANSWERED: &str = "The user didn't answer these questions. Their message follows.";

    /// `ask_user`'s `questions`, checked against the limits. An error is
    /// the call's result, worded for the model to fix and call again.
    pub fn parse_questions(input: &serde_json::Value) -> Result<Vec<Question>, String> {
        let questions = input
            .get("questions")
            .ok_or_else(|| "missing field: questions".to_string())?;
        let questions: Vec<Question> =
            serde_json::from_value(questions.clone()).map_err(|e| format!("invalid questions: {e}"))?;
        if questions.is_empty() || questions.len() > MAX_QUESTIONS {
            return Err(format!(
                "ask 1 to {MAX_QUESTIONS} questions in one call (got {})",
                questions.len()
            ));
        }
        for (i, q) in questions.iter().enumerate() {
            let n = i + 1;
            if q.question.trim().is_empty() {
                return Err(format!("question {n} has an empty question"));
            }
            if q.header.trim().is_empty() {
                return Err(format!("question {n} has an empty header"));
            }
            if q.header.chars().count() > MAX_HEADER_CHARS {
                return Err(format!("question {n}'s header is longer than {MAX_HEADER_CHARS} characters"));
            }
            if !q.options.is_empty() && !(MIN_OPTIONS..=MAX_OPTIONS).contains(&q.options.len()) {
                return Err(format!(
                    "question {n} offers {} options: offer {MIN_OPTIONS} to {MAX_OPTIONS}, or none for free text",
                    q.options.len()
                ));
            }
            if q.options.iter().any(|o| o.label.trim().is_empty()) {
                return Err(format!("question {n} has an option with an empty label"));
            }
            for (j, option) in q.options.iter().enumerate() {
                if q.options[..j].iter().any(|o| o.label == option.label) {
                    return Err(format!("question {n}'s option labels must be unique ({:?} repeats)", option.label));
                }
            }
            if q.multi_select && q.options.is_empty() {
                return Err(format!("question {n} sets multi_select but offers no options"));
            }
        }
        Ok(questions)
    }

    /// Whether `answers` answers `questions`: one answer per question, only
    /// offered labels, at most one unless multi-select, and something
    /// chosen or written for each.
    pub fn validate_answers(questions: &[Question], answers: &[QuestionAnswer]) -> Result<(), String> {
        if answers.len() != questions.len() {
            return Err(format!(
                "answer all {} questions (got {} answers)",
                questions.len(),
                answers.len()
            ));
        }
        for (q, a) in questions.iter().zip(answers) {
            for label in &a.selected {
                if !q.options.iter().any(|o| &o.label == label) {
                    return Err(format!("{:?} isn't one of the choices for {:?}", label, q.header));
                }
            }
            if !q.multi_select && a.selected.len() > 1 {
                return Err(format!("choose one option for {:?}", q.header));
            }
            let wrote = a.other.as_deref().is_some_and(|t| !t.trim().is_empty());
            if a.selected.is_empty() && !wrote {
                return Err(format!("choose or write an answer for {:?}", q.header));
            }
        }
        Ok(())
    }

    /// The `ask_user` result the model gets for `answers`, one line per
    /// question.
    pub fn answered_result(questions: &[Question], answers: &[QuestionAnswer]) -> String {
        let mut result = String::from("The user answered:");
        for (i, (q, a)) in questions.iter().zip(answers).enumerate() {
            let wrote = a.other.as_deref().map(str::trim).filter(|t| !t.is_empty());
            let answer = match (a.selected.is_empty(), wrote) {
                (false, Some(text)) => format!("{}; they also wrote: {text:?}", a.selected.join(", ")),
                (false, None) => a.selected.join(", "),
                (true, Some(text)) => format!("they wrote: {text:?}"),
                (true, None) => "no answer".to_string(),
            };
            result.push_str(&format!("\n{}. {}: {answer}", i + 1, q.header));
        }
        result
    }
}

#[cfg(feature = "server")]
pub use server::*;

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use serde_json::json;

    fn yes_no(header: &str) -> serde_json::Value {
        json!({
            "question": "Delete the file?",
            "header": header,
            "options": [{"label": "Yes"}, {"label": "No", "description": "keep it"}]
        })
    }

    #[test]
    fn test_parse_questions_accepts_options_and_free_text() {
        let input = json!({"questions": [
            yes_no("Delete"),
            {"question": "Anything else?", "header": "Notes"},
            {"question": "Which?", "header": "Pick", "multi_select": true,
             "options": [{"label": "A"}, {"label": "B"}, {"label": "C"}]}
        ]});
        let questions = parse_questions(&input).expect("valid input");
        assert_eq!(questions.len(), 3);
        assert_eq!(questions[0].options[1].description.as_deref(), Some("keep it"));
        assert!(questions[1].options.is_empty());
        assert!(questions[2].multi_select);
    }

    #[test]
    fn test_parse_questions_refuses_each_broken_limit() {
        let five: Vec<_> = (0..5).map(|i| yes_no(&format!("Q{i}"))).collect();
        let cases = [
            (json!({}), "questions"),
            (json!({"questions": []}), "1 to 4"),
            (json!({"questions": five}), "1 to 4"),
            (json!({"questions": [{"question": " ", "header": "H"}]}), "question"),
            (json!({"questions": [{"question": "Q?", "header": ""}]}), "header"),
            (json!({"questions": [{"question": "Q?", "header": "x".repeat(31)}]}), "30"),
            (json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "A"}]}]}), "2 to 4"),
            (
                json!({"questions": [{"question": "Q?", "header": "H",
                    "options": [{"label": "A"}, {"label": "B"}, {"label": "C"}, {"label": "D"}, {"label": "E"}]}]}),
                "2 to 4",
            ),
            (json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "A"}, {"label": "A"}]}]}), "unique"),
            (json!({"questions": [{"question": "Q?", "header": "H", "options": [{"label": "A"}, {"label": " "}]}]}), "label"),
            (json!({"questions": [{"question": "Q?", "header": "H", "multi_select": true}]}), "multi_select"),
        ];
        for (input, expected) in cases {
            let err = parse_questions(&input).expect_err(&format!("{input} should be refused"));
            assert!(err.contains(expected), "{input}: {err:?} should mention {expected:?}");
        }
    }

    fn questions() -> Vec<Question> {
        let input = json!({"questions": [
            yes_no("Delete"),
            {"question": "Anything else?", "header": "Notes"},
            {"question": "Which?", "header": "Pick", "multi_select": true,
             "options": [{"label": "A"}, {"label": "B"}, {"label": "C"}]}
        ]});
        parse_questions(&input).expect("valid input")
    }

    fn answer(selected: &[&str], other: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            selected: selected.iter().map(|s| s.to_string()).collect(),
            other: other.map(str::to_string),
        }
    }

    #[test]
    fn test_validate_answers_accepts_a_full_answer() {
        let answers = [answer(&["Yes"], None), answer(&[], Some("no")), answer(&["A", "C"], Some("and D"))];
        validate_answers(&questions(), &answers).expect("a full answer");
    }

    #[test]
    fn test_validate_answers_refuses_each_bad_answer() {
        let ok = || vec![answer(&["Yes"], None), answer(&[], Some("no")), answer(&["A"], None)];
        let mut cases: Vec<(Vec<QuestionAnswer>, &str)> = Vec::new();
        cases.push((ok()[..2].to_vec(), "3"));
        let mut c = ok();
        c[0] = answer(&["Maybe"], None);
        cases.push((c, "Maybe"));
        let mut c = ok();
        c[0] = answer(&["Yes", "No"], None);
        cases.push((c, "one"));
        let mut c = ok();
        c[1] = answer(&[], Some("  "));
        cases.push((c, "Notes"));
        let mut c = ok();
        c[2] = answer(&[], None);
        cases.push((c, "Pick"));
        let mut c = ok();
        c[1] = answer(&["Yes"], Some("x"));
        cases.push((c, "Yes"));
        for (answers, expected) in cases {
            let err = validate_answers(&questions(), &answers).expect_err(&format!("{answers:?} should be refused"));
            assert!(err.contains(expected), "{answers:?}: {err:?} should mention {expected:?}");
        }
    }

    #[test]
    fn test_answered_result_has_one_line_per_question() {
        let answers = [answer(&["Yes"], None), answer(&[], Some("ship it")), answer(&["A", "C"], Some("and D"))];
        assert_eq!(
            answered_result(&questions(), &answers),
            "The user answered:\n\
             1. Delete: Yes\n\
             2. Notes: they wrote: \"ship it\"\n\
             3. Pick: A, C; they also wrote: \"and D\""
        );
    }

    #[test]
    fn test_question_types_round_trip_through_json() {
        let pending = PendingQuestion { tool_use_id: "toolu_1".to_string(), questions: questions() };
        let json = serde_json::to_string(&pending).expect("serialize");
        assert_eq!(serde_json::from_str::<PendingQuestion>(&json).expect("deserialize"), pending);
        let answers = vec![answer(&["Yes"], None), answer(&[], Some("x"))];
        let json = serde_json::to_string(&answers).expect("serialize");
        assert_eq!(serde_json::from_str::<Vec<QuestionAnswer>>(&json).expect("deserialize"), answers);
    }
}
