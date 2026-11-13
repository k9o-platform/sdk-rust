//! The select builder's `vector_search` sends the embedding in a POST body to `data/<collection>/search`;
//! a select without it stays a GET.

use k9o::{Client, ClientOpts, TextSearchOptions, VectorMetric, VectorSearchOptions};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": [{"id": "t1", "status": "open"}], "meta": {"cursor": {"next": null}}}),
        ))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

async fn last(server: &MockServer) -> (String, String, Value) {
    let reqs = server.received_requests().await.unwrap();
    let r = reqs.last().unwrap();
    let body = if r.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&r.body).unwrap()
    };
    (r.method.to_string(), r.url.path().to_string(), body)
}

#[tokio::test]
async fn vector_only_posts_body() {
    let (server, c) = setup().await;
    let v = vec![0.25; 768];
    let resp = c
        .from("tickets")
        .select()
        .vector_search("embedding", &v, VectorSearchOptions::default())
        .execute::<Value>()
        .await
        .unwrap();
    assert_eq!(resp.data[0]["id"], "t1");
    assert!(resp.meta.cursor.unwrap().next.is_none());
    let (m, p, b) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("POST", "/@portal/data/tickets/search")
    );
    assert_eq!(
        b,
        json!({"vectorSearch": {"column": "embedding", "vector": v}})
    );
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.query(), None);
}

#[tokio::test]
async fn filters_and_limit_go_in_body() {
    let (server, c) = setup().await;
    c.from("tickets")
        .select()
        .eq("status", "open")
        .gt("priority", 2)
        .limit(20)
        .cursor("c1")
        .count("exact")
        .vector_search("embedding", &[1.0, 2.0], VectorSearchOptions::default())
        .execute::<Value>()
        .await
        .unwrap();
    let (m, p, b) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("POST", "/@portal/data/tickets/search")
    );
    assert_eq!(
        b,
        json!({
            "vectorSearch": {"column": "embedding", "vector": [1.0, 2.0]},
            "filter": {"status": "eq.open", "priority": "gt.2"},
            "limit": 20, "cursor": "c1", "count": "exact"
        })
    );
}

#[tokio::test]
async fn metric_and_vector_limit() {
    let (server, c) = setup().await;
    c.from("tickets")
        .select()
        .limit(5)
        .vector_search(
            "embedding",
            &[0.5],
            VectorSearchOptions {
                metric: VectorMetric::Euclidean,
                limit: Some(5),
            },
        )
        .execute::<Value>()
        .await
        .unwrap();
    let (_, _, b) = last(&server).await;
    assert_eq!(
        b,
        json!({"vectorSearch": {"column": "embedding", "vector": [0.5], "metric": "euclidean", "limit": 5}, "limit": 5})
    );
}

#[tokio::test]
async fn text_search_combination_rejected_without_request() {
    let (server, c) = setup().await;
    let e = c
        .from("tickets")
        .select()
        .text_search("body", "x", TextSearchOptions::default())
        .vector_search("embedding", &[1.0], VectorSearchOptions::default())
        .execute::<Value>()
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert!(e.message.contains("cannot be combined"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn plain_select_is_get() {
    let (server, c) = setup().await;
    c.from("tickets")
        .select()
        .eq("status", "open")
        .execute::<Value>()
        .await
        .unwrap();
    let (m, p, _) = last(&server).await;
    assert_eq!((m.as_str(), p.as_str()), ("GET", "/@portal/data/tickets"));
}
