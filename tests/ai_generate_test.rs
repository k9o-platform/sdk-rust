//! `client.ai().generate()` — URL, verb, body shape (prompt first, unset fields omitted),
//! result parsing, bound-project guard and 400/501 error mapping.

use k9o::{Client, ClientOpts, GenerateOptions};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer, slug: Option<&str>) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: slug.map(String::from),
        ..Default::default()
    })
    .unwrap()
}

fn ok_body() -> Value {
    json!({"data": {"modelId": "llama-3-8b", "content": "Bonjour", "finishReason": "stop",
        "usage": {"promptTokens": 5, "completionTokens": 2, "totalTokens": 7}, "usageType": "chat"}, "meta": {}})
}

async fn mount(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path("/@portal/ai/generate"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn last_raw(server: &MockServer) -> String {
    let reqs = server.received_requests().await.unwrap();
    String::from_utf8(reqs.last().unwrap().body.clone()).unwrap()
}

#[tokio::test]
async fn generate_posts_prompt_only_and_parses_result() {
    let server = MockServer::start().await;
    mount(&server, 200, ok_body()).await;
    let r = client(&server, Some("portal"))
        .ai()
        .generate("hi", GenerateOptions::default())
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/@portal/ai/generate");
    assert_eq!(last_raw(&server).await, r#"{"prompt":"hi"}"#);
    assert_eq!(r.data.model_id, "llama-3-8b");
    assert_eq!(r.data.content, "Bonjour");
    assert_eq!(r.data.finish_reason.as_deref(), Some("stop"));
    assert_eq!(r.data.usage.total_tokens, 7);
    assert_eq!(r.data.usage_type, "chat");
}

#[tokio::test]
async fn generate_sends_given_options_prompt_first() {
    let server = MockServer::start().await;
    mount(&server, 200, ok_body()).await;
    client(&server, Some("portal"))
        .ai()
        .generate(
            "hi",
            GenerateOptions {
                model: Some("m"),
                max_tokens: Some(9),
                temperature: Some(0.0),
                usage_type: Some("chat"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last_raw(&server).await,
        r#"{"prompt":"hi","model":"m","maxTokens":9,"temperature":0.0,"usageType":"chat"}"#
    );
}

#[tokio::test]
async fn generate_requires_bound_project() {
    let server = MockServer::start().await;
    mount(&server, 200, ok_body()).await;
    let e = client(&server, None)
        .ai()
        .generate("x", GenerateOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn generate_maps_400_and_501() {
    for (status, code) in [
        (400u16, "K9O_VALIDATION_FAILED"),
        (501, "K9O_NOT_IMPLEMENTED"),
    ] {
        let server = MockServer::start().await;
        mount(&server, status, json!({"code": code, "message": "no"})).await;
        let e = client(&server, Some("portal"))
            .ai()
            .generate("hi", GenerateOptions::default())
            .await
            .unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(e.status, Some(status));
    }
}
