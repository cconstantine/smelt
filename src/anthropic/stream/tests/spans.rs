use super::*;

/// SME-137: a model call is one CLIENT span named after its model, with
/// its usage, finish reason and retries, and none of its text.
#[tokio::test]
async fn test_a_model_call_is_a_client_span_with_its_usage_and_retries() {
    let ((result, _), spans) = crate::telemetry::capture_spans(run_against_responses(vec![
        (529, PAUSED_PROVIDER_BODY),
        (200, USAGE_BODY),
    ]))
    .await;
    assert!(result.is_ok(), "{result:?}");
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {spans:?}") };
    assert_eq!(span.name, "chat claude-opus-4-8");
    assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Client);
    assert_eq!(span_attribute(span, "gen_ai.operation.name"), Some(&"chat".into()));
    assert_eq!(span_attribute(span, "gen_ai.request.model"), Some(&"claude-opus-4-8".into()));
    assert_eq!(span_attribute(span, "server.address"), Some(&"127.0.0.1".into()));
    assert_eq!(span_attribute(span, "gen_ai.usage.input_tokens"), Some(&120_i64.into()));
    assert_eq!(span_attribute(span, "gen_ai.usage.output_tokens"), Some(&42_i64.into()));
    assert_eq!(span_attribute(span, "gen_ai.usage.cache_creation.input_tokens"), Some(&7_i64.into()));
    assert_eq!(span_attribute(span, "gen_ai.usage.cache_read.input_tokens"), Some(&30_i64.into()));
    assert_eq!(span_attribute(span, "gen_ai.response.finish_reasons"), Some(&"end_turn".into()));
    assert_eq!(span_attribute(span, "smelt.retries"), Some(&1_i64.into()));
    assert_eq!(span_attribute(span, "smelt.binding"), Some(&"AsIs".into()));
    assert_eq!(span.status, opentelemetry::trace::Status::Unset);
    assert!(!format!("{span:?}").contains("secret reply"), "no reply text: {span:?}");
}

#[tokio::test]
async fn test_a_refused_model_call_marks_its_span_failed() {
    let ((result, _), spans) = crate::telemetry::capture_spans(run_against_responses(vec![(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
    )]))
    .await;
    assert!(result.is_err());
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {:?}", spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>()) };
    let opentelemetry::trace::Status::Error { description } = &span.status else {
        panic!("{:?}", span.status)
    };
    assert!(description.contains("bad"), "{description}");
}

#[tokio::test]
async fn test_a_cut_off_model_call_marks_its_span_failed() {
    const CUT_OFF_BODY: &str = concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
    );
    let ((result, _), spans) =
        crate::telemetry::capture_spans(run_against_responses(vec![(200, CUT_OFF_BODY)])).await;
    assert!(result.is_ok(), "{result:?}");
    let chats = chat_spans(&spans);
    let [span] = chats.as_slice() else { panic!("one chat span: {spans:?}") };
    assert!(matches!(span.status, opentelemetry::trace::Status::Error { .. }), "{:?}", span.status);
    assert_eq!(span_attribute(span, "gen_ai.response.finish_reasons"), Some(&"max_tokens".into()));
}
