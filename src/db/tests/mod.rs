use super::*;

fn sample_questions() -> Vec<crate::questions::Question> {
    crate::questions::parse_questions(&serde_json::json!({"questions": [
        {"question": "Delete it?", "header": "Delete", "options": [{"label": "Yes"}, {"label": "No"}]}
    ]}))
    .expect("valid questions")
}

fn sample_answer(label: &str) -> Vec<crate::questions::QuestionAnswer> {
    vec![crate::questions::QuestionAnswer { selected: vec![label.to_string()], other: None }]
}

// --- Terminal ---

async fn test_conversation(pool: &PgPool) -> Conversation {
    create_conversation(pool)
        .await
        .expect("create conversation")
}

/// A pod + terminal pair, for tests that only care about
/// `terminal_commands`/`terminal_events` and just need a valid
/// `terminal_id` to hang them off — most of this module.
async fn test_terminal(pool: &PgPool, conversation_id: i64) -> i64 {
    // One live pod per conversation (SME-51 B7): reuse it if there is one.
    let live = list_sandbox_pods(pool, conversation_id)
        .await
        .expect("list pods")
        .into_iter()
        .find(|p| p.terminated_at.is_none());
    let pod = match live {
        Some(pod) => pod,
        None => create_sandbox_pod(pool, conversation_id).await.expect("create sandbox pod"),
    };
    let terminal = create_sandbox_terminal(pool, pod.id)
        .await
        .expect("create sandbox terminal");
    terminal.id
}

// --- Model providers (SME-72) ---

async fn test_provider(pool: &PgPool, name: &str) -> InferenceProvider {
    create_inference_provider(pool, name, "anthropic", "https://api.anthropic.com", "api_key", "sk-ant-0123456789", false, None, true, None)
        .await
        .expect("create provider")
}

fn usage(input: i64, output: i64, write: i64, read: i64) -> crate::anthropic::TokenUsage {
    crate::anthropic::TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: write,
        cache_read_input_tokens: read,
    }
}

mod language_servers;
mod questions;
mod conversations;
mod pods;
mod commands;
mod mcp;
mod git;
mod volumes;
mod usage;
mod providers;
mod instance;
