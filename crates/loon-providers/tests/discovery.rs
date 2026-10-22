use loon_providers::{Endpoint, HttpTransport, ProviderError};
use std::time::Duration;
#[allow(dead_code)]
mod support;
#[tokio::test]
async fn model_discovery_paginates_normalizes_and_deduplicates() {
    let server = support::Fixture::start(vec![
        (
            200,
            "application/json",
            br#"{"data":[{"id":"a"}],"has_more":true,"last_id":"a"}"#.to_vec(),
        ),
        (
            200,
            "application/json",
            br#"{"data":[{"id":"a"},{"id":"b"}],"has_more":false}"#.to_vec(),
        ),
    ])
    .await;
    let endpoint = Endpoint::from_parts(
        &format!("{}/chat/completions", server.base_url),
        None,
        &Default::default(),
        Duration::from_secs(2),
    );
    assert_eq!(endpoint.base_url, server.base_url);
    assert_eq!(
        HttpTransport::new()
            .unwrap()
            .list_models(&endpoint)
            .await
            .unwrap(),
        vec!["a", "b"]
    );
}
#[tokio::test]
async fn authentication_failures_are_classified_correctly() {
    let server = support::Fixture::start(vec![(
        401,
        "application/json",
        br#"{"error":"bad key"}"#.to_vec(),
    )])
    .await;
    let endpoint = Endpoint::from_parts(
        &server.base_url,
        None,
        &Default::default(),
        Duration::from_secs(2),
    );
    assert!(matches!(
        HttpTransport::new().unwrap().list_models(&endpoint).await,
        Err(ProviderError::Auth(_))
    ));
}
#[tokio::test]
async fn repeated_pagination_cursors_fail_instead_of_looping() {
    let server = support::Fixture::start(vec![(
        200,
        "application/json",
        br#"{"data":[{"id":"a"}],"has_more":true,"last_id":"a"}"#.to_vec(),
    )])
    .await;
    let endpoint = Endpoint::from_parts(
        &server.base_url,
        None,
        &Default::default(),
        Duration::from_secs(2),
    );
    let error = HttpTransport::new()
        .unwrap()
        .list_models(&endpoint)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cursor repeated"), "{error}");
}
#[test]
fn reasoning_parameters_use_the_correct_wire_shape() {
    let request = loon_providers::ChatRequest {
        model: "test".into(),
        reasoning_effort: Some("xhigh".into()),
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    let chat = loon_providers::chat::ChatCompletionsAdapter::build_body(&request);
    assert_eq!(chat["reasoning_effort"], "xhigh");
    assert_eq!(chat["max_completion_tokens"], 4096);
    assert!(chat.get("max_tokens").is_none());
    let responses = loon_providers::responses::ResponsesAdapter::build_body(&request);
    assert_eq!(responses["reasoning"]["effort"], "xhigh");
    assert_eq!(responses["max_output_tokens"], 4096);
}
