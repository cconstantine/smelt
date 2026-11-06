//! The loopback MCP bridge (SME-430), called the way the CLI calls it: an
//! rmcp client over streamable HTTP, with the CLI's `_meta` tool-use id.

use crate::claude_cli::bridge::{ParkedCall, ToolReply, bridge, cli_tool_name};

pub(crate) type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Connects to a run's server as the CLI does, with `token`.
pub(crate) async fn connect(url: &str, token: &str) -> Result<Client, String> {
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    crate::mcp::install_crypto_provider();
    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string()).auth_header(token.to_string());
    let transport = StreamableHttpClientTransport::with_client(reqwest_rmcp::Client::new(), config);
    ().serve(transport).await.map_err(|e| e.to_string())
}

/// Calls `name` (the CLI's name for it) with `arguments`, under the
/// CLI's `_meta` tool-use id when one is given: the result's text, and
/// whether it's an error.
pub(crate) async fn call(client: &rmcp::service::Peer<rmcp::RoleClient>, name: &str, arguments: serde_json::Value, tool_use_id: Option<&str>) -> (String, bool) {
    let mut params = rmcp::model::CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    if let Some(id) = tool_use_id {
        let mut meta = rmcp::model::RequestMetaObject::new();
        meta.insert(crate::claude_cli::bridge::TOOL_USE_ID_META.to_string(), serde_json::json!(id));
        params.meta = Some(meta);
    }
    let result = client.call_tool(params).await.expect("the call is answered");
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    (text, result.is_error == Some(true))
}

/// `future`, failing the test if it takes over 10 s: every wait on the
/// code under test is bounded (tdd.md).
pub(crate) async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(10), future).await.expect("answered within 10 s")
}

fn tool(name: &str) -> crate::anthropic::ToolDefinition {
    crate::anthropic::ToolDefinition {
        name: name.to_string(),
        description: format!("The {name} tool."),
        input_schema: serde_json::json!({"type": "object", "properties": {"key": {"type": "string"}}}),
    }
}

#[test]
fn test_cli_tool_names_are_plain() {
    assert_eq!(cli_tool_name("todoread"), "mcp__smelt__todoread");
    assert_eq!(cli_tool_name("github.create_issue"), "mcp__smelt__github_create_issue");
}

#[tokio::test]
async fn test_a_call_parks_under_its_tool_use_id_until_smelt_answers() {
    let bridge = bridge().await.expect("the bridge starts");
    let mut run = bridge.register(&[tool("todoread"), tool("github.create_issue")]);
    let client = connect(&run.url, &run.token).await.expect("the CLI connects");
    let listed: Vec<String> = client.peer().list_all_tools().await.expect("tools").into_iter().map(|t| t.name.to_string()).collect();
    assert_eq!(listed, ["todoread", "github_create_issue"], "the CLI prefixes mcp__smelt__ itself");

    let calling = tokio::spawn(async move {
        let answered = call(client.peer(), "github_create_issue", serde_json::json!({"key": "a"}), Some("toolu_01")).await;
        (answered, client)
    });
    let parked: ParkedCall = within(run.calls.recv()).await.expect("the call parks");
    assert_eq!(parked.tool_use_id.as_deref(), Some("toolu_01"));
    assert_eq!(parked.name, "github.create_issue", "smelt's own name, mapped back");
    assert_eq!(parked.arguments, serde_json::json!({"key": "a"}));
    assert!(!calling.is_finished(), "nothing answers but smelt");
    parked.reply.send(ToolReply { text: vec!["value-42".to_string()], is_error: false }).expect("answer");
    let ((text, is_error), _client) = within(calling).await.expect("the call returns");
    assert_eq!((text.as_str(), is_error), ("value-42", false));
}

#[tokio::test]
async fn test_a_run_that_ends_answers_its_parked_calls_and_refuses_more() {
    let bridge = bridge().await.expect("the bridge starts");
    let mut run = bridge.register(&[tool("todoread")]);
    let client = connect(&run.url, &run.token).await.expect("the CLI connects");
    let calling = tokio::spawn(async move { call(client.peer(), "todoread", serde_json::json!({}), None).await });
    let parked = within(run.calls.recv()).await.expect("the call parks");
    assert_eq!(parked.tool_use_id, None, "a call without the meta still parks");
    let url = run.url.clone();
    drop(parked);
    drop(run);
    let (text, is_error) = within(calling).await.expect("the call returns");
    assert!(is_error);
    assert_eq!(text, crate::claude_cli::bridge::ENDED);
    assert!(connect(&url, "anything").await.is_err(), "the run's path is gone");
}

#[tokio::test]
async fn test_the_bridge_refuses_a_wrong_token_and_an_unknown_tool() {
    let bridge = bridge().await.expect("the bridge starts");
    let run = bridge.register(&[tool("todoread")]);
    assert!(connect(&run.url, "not-the-token").await.is_err(), "a wrong token is refused");
    let client = connect(&run.url, &run.token).await.expect("the CLI connects");
    let (text, is_error) = call(client.peer(), "rm_rf", serde_json::json!({}), Some("toolu_02")).await;
    assert!(is_error);
    assert!(text.contains("no tool named rm_rf"), "{text}");
}
