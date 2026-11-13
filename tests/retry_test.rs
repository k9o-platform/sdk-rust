//! Retry + backoff behaviour. Idempotent methods are retried
//! on transient failures (transport / 502 / 503 / 504 / 429); semantic 4xx
//! and non-idempotent methods are not. Backoff::None ⇒ no sleeps.

use k9o::{Backoff, Client, ClientOpts, CreateProjectInput, RetryConfig};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn retry_client(server: &MockServer, max_retries: u32) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        retry_config: Some(RetryConfig {
            max_retries,
            backoff: Backoff::None,
            initial_delay_ms: 0,
            max_delay_ms: 0,
        }),
        ..Default::default()
    })
    .unwrap()
}

fn kerr_body() -> serde_json::Value {
    json!({"code": "K9O_UPSTREAM_UNAVAILABLE", "message": "down"})
}

#[tokio::test]
async fn retry_get_exhausts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs"))
        .respond_with(ResponseTemplate::new(503).set_body_json(kerr_body()))
        .expect(3) // 1 initial + 2 retries
        .mount(&server)
        .await;
    let res = retry_client(&server, 2).orgs().list().await;
    assert!(res.is_err());
}

#[tokio::test]
async fn retry_post_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/projects"))
        .respond_with(ResponseTemplate::new(503).set_body_json(kerr_body()))
        .expect(1) // non-idempotent → single attempt
        .mount(&server)
        .await;
    let res = retry_client(&server, 2)
        .projects()
        .create(CreateProjectInput {
            slug: Some("valid-slug"),
            display_name: "X",
            ..Default::default()
        })
        .await;
    assert!(res.is_err());
}

#[tokio::test]
async fn retry_get_semantic_4xx_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/orgs"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"code": "K9O_NOT_FOUND", "message": "nope"})),
        )
        .expect(1) // 404 is semantic, not transient
        .mount(&server)
        .await;
    let res = retry_client(&server, 2).orgs().list().await;
    assert!(res.is_err());
}

#[tokio::test]
async fn retry_get_succeeds_after_transient() {
    let server = MockServer::start().await;
    // 503 for the first call only (higher priority), then 200.
    Mock::given(method("GET"))
        .and(path("/orgs"))
        .respond_with(ResponseTemplate::new(503).set_body_json(kerr_body()))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/orgs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "meta": {}})))
        .mount(&server)
        .await;
    let res = retry_client(&server, 3).orgs().list().await;
    assert!(res.is_ok(), "expected success after one transient 503");
}
