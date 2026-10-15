use super::*;

#[tokio::test]
async fn test_a_transient_provider_error_is_retried() {
    let (result, requests) =
        run_against_responses(vec![(503, PAUSED_PROVIDER_BODY), (200, OK_BODY)]).await;
    assert!(result.is_ok(), "should recover on retry: {result:?}");
    assert_eq!(requests, 2);
}

#[tokio::test]
async fn test_a_client_error_is_not_retried() {
    let (result, requests) =
        run_against_responses(vec![(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#)]).await;
    assert!(result.is_err());
    assert_eq!(requests, 1, "a 400 won't get better by retrying");
}
