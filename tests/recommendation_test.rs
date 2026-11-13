//! `recommendation` on a gateway error body may be null, a string or an
//! object. Decoding never fails because of it, and the rest of the error
//! survives.

use k9o::{Backoff, Client, ClientOpts, KError, RetryConfig};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        org_slug: Some("acme".into()),
        retry_config: Some(RetryConfig {
            max_retries: 0,
            backoff: Backoff::None,
            initial_delay_ms: 0,
            max_delay_ms: 0,
        }),
        ..Default::default()
    })
    .unwrap()
}

fn body(rec: Option<Value>) -> Value {
    let mut b =
        json!({"code": "K9O_READ_ONLY_SUSPENDED", "message": "suspended", "context": {"x": 1}});
    if let Some(r) = rec {
        b["recommendation"] = r;
    }
    b
}

async fn decode(status: u16, rec: Option<Value>) -> KError {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body(rec)))
        .mount(&server)
        .await;
    client(&server).projects().list().await.unwrap_err()
}

fn check(e: &KError, status: u16, rec: Option<&str>, retry: Option<f64>) {
    assert_eq!(e.code, "K9O_READ_ONLY_SUSPENDED");
    assert_eq!(e.message, "suspended");
    assert_eq!(e.status, Some(status));
    assert_eq!(e.context.get("x"), Some(&json!(1)));
    assert_eq!(e.recommendation.as_deref(), rec);
    assert_eq!(e.retry_after_sec, retry);
}

#[tokio::test]
async fn object_with_action() {
    let e = decode(
        403,
        Some(json!({"action": "Open Billing and settle the balance."})),
    )
    .await;
    check(&e, 403, Some("Open Billing and settle the balance."), None);
}

#[tokio::test]
async fn object_with_retry_after_and_note() {
    let e = decode(429, Some(json!({"retryAfterSec": 30, "note": "retry"}))).await;
    check(&e, 429, Some("retry"), Some(30.0));
}

#[tokio::test]
async fn action_wins_over_note_and_fraction_kept() {
    let e = decode(
        429,
        Some(json!({"action": "a", "note": "n", "retryAfterSec": 1.5})),
    )
    .await;
    check(&e, 429, Some("a"), Some(1.5));
}

#[tokio::test]
async fn null_missing_and_string() {
    check(&decode(403, Some(Value::Null)).await, 403, None, None);
    check(&decode(403, None).await, 403, None, None);
    check(
        &decode(403, Some(json!("do this"))).await,
        403,
        Some("do this"),
        None,
    );
}

#[tokio::test]
async fn other_json_types_give_none() {
    check(&decode(403, Some(json!(7))).await, 403, None, None);
    check(&decode(403, Some(json!(true))).await, 403, None, None);
    check(&decode(403, Some(json!(["a", "b"]))).await, 403, None, None);
}

#[tokio::test]
async fn bad_retry_after_values_give_none() {
    for v in [json!(-1), json!("30"), json!(null), json!({"x": 1})] {
        let e = decode(429, Some(json!({"retryAfterSec": v, "action": "w"}))).await;
        check(&e, 429, Some("w"), None);
    }
    let e = decode(429, Some(json!({"retryAfterSec": 0}))).await;
    check(&e, 429, None, Some(0.0));
}

#[tokio::test]
async fn object_without_text_gives_none() {
    let e = decode(403, Some(json!({"action": 5, "note": null}))).await;
    check(&e, 403, None, None);
}

#[tokio::test]
async fn stream_error_path_decodes_object() {
    use k9o::{LlmInvokeInput, LlmInvokeParams};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@portal/compute/llms/gpt-4o/invoke-stream"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(body(Some(json!({"retryAfterSec": 30, "note": "retry"})))),
        )
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        org_slug: Some("acme".into()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c
        .compute()
        .llms()
        .invoke_stream(
            "gpt-4o",
            LlmInvokeInput {
                prompt: "hi",
                params: LlmInvokeParams::default(),
            },
        )
        .await
        .err()
        .expect("error");
    check(&e, 429, Some("retry"), Some(30.0));
}
