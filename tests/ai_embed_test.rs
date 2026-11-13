//! `client.ai().embed()` — URL, verb, body shape, bound-project guard, and
//! that errors never echo the input.

use k9o::{Client, ClientOpts, EmbedInput, EmbedText};
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
    json!({"data": {"embeddings": [[0.1, 0.2]], "model": "nomic-embed-text-v1.5", "dimensions": 2}, "meta": {}})
}

async fn mount_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/@portal/ai/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(server)
        .await;
}

async fn last_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.unwrap();
    serde_json::from_slice(&reqs.last().unwrap().body).unwrap()
}

#[tokio::test]
async fn embed_posts_url_and_string_input() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let r = client(&server, Some("portal"))
        .ai()
        .embed(EmbedInput {
            model: None,
            input: "hello".into(),
        })
        .await
        .unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/@portal/ai/embeddings");
    assert_eq!(last_body(&server).await, json!({"input": "hello"}));
    assert_eq!(r.data.embeddings, vec![vec![0.1, 0.2]]);
    assert_eq!(r.data.dimensions, 2);
}

#[tokio::test]
async fn embed_sends_list_and_model_as_given() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    client(&server, Some("portal"))
        .ai()
        .embed(EmbedInput {
            model: Some("nomic-embed-text-v1.5".into()),
            input: EmbedText::from(vec!["search_document: a", "search_document: b"]),
        })
        .await
        .unwrap();
    assert_eq!(
        last_body(&server).await,
        json!({"model": "nomic-embed-text-v1.5", "input": ["search_document: a", "search_document: b"]})
    );
}

#[tokio::test]
async fn embed_omits_model_when_not_given() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    client(&server, Some("portal"))
        .ai()
        .embed(EmbedInput {
            model: None,
            input: vec!["x"].into(),
        })
        .await
        .unwrap();
    let b = last_body(&server).await;
    assert!(b.as_object().unwrap().get("model").is_none());
    assert_eq!(b, json!({"input": ["x"]}));
}

#[tokio::test]
async fn embed_requires_bound_project() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let e = client(&server, None)
        .ai()
        .embed(EmbedInput {
            model: None,
            input: "x".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn embed_surfaces_refusal_as_kerror_without_input() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@portal/ai/embeddings"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "K9O_MODEL_NOT_DECLARED",
            "message": "model is not a declared embedding model",
            "context": {"declared": ["nomic-embed-text-v1.5"]},
            "recommendation": null
        })))
        .mount(&server)
        .await;
    let secret = "the-secret-document-body";
    let e = client(&server, Some("portal"))
        .ai()
        .embed(EmbedInput {
            model: Some("other".into()),
            input: secret.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MODEL_NOT_DECLARED");
    assert!(!e.to_string().contains(secret));
    assert!(!format!("{:?}", e).contains(secret));
}
