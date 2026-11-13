//! User data streams tests: exact method/path/body per call, subscribe resume
//! across a server-closed connection, and the 503 mapping.

use k9o::{Client, ClientOpts};
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap()
}

fn env(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"data": data, "meta": {}}))
}

fn frame(seq: &str) -> String {
    let body = json!({"sequence": seq, "subject": "s", "data": {"n": seq}, "publishedAt": "t"});
    format!("id: {}\ndata: {}\n\n", seq, body)
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

#[tokio::test]
async fn control_plane_and_data_plane_requests() {
    let server = MockServer::start().await;
    Mock::given(any_request())
        .respond_with(|req: &wiremock::Request| {
            let p = req.url.path();
            if req.method.as_str() == "DELETE" {
                env(json!({"slug": "orders", "status": "deleted"}))
            } else if p.ends_with("/publish") {
                env(json!({"streamSlug": "orders", "subject": "k9o.x", "bytes": 8, "accepted": true}))
            } else if p.ends_with("/replay") {
                env(json!({"streamSlug": "orders", "events": [{"sequence": "11", "subject": "s", "data": {"id": 1}, "publishedAt": "t"}], "nextCursor": "11", "hasMore": false}))
            } else if req.method.as_str() == "GET" && p.ends_with("/user") {
                env(json!([{"id": "pstr_1", "slug": "orders", "displayName": "Orders", "maxBytes": 1073741824}]))
            } else {
                env(json!({"id": "pstr_1", "slug": "orders", "displayName": "Orders", "maxBytes": null}))
            }
        })
        .mount(&server)
        .await;
    let c = client(&server);

    let created = c
        .streams()
        .create("Orders", 1073741824, None)
        .await
        .unwrap();
    assert_eq!(created.data.slug, "orders");
    assert_eq!(created.data.max_bytes, None);
    c.streams()
        .create("Orders", 1073741824, Some(86400))
        .await
        .unwrap();
    let listed = c.streams().list().await.unwrap();
    assert_eq!(listed.data[0].max_bytes, Some(1073741824));
    c.streams().get("orders").await.unwrap();
    let del = c.streams().delete("orders").await.unwrap();
    assert_eq!(del.data.status, "deleted");
    let pub_ = c
        .streams()
        .publish("orders", &json!({"id": 1}), Some("eu.new"))
        .await
        .unwrap();
    assert!(pub_.data.accepted);
    c.streams()
        .publish("orders", &json!(1), None)
        .await
        .unwrap();
    let rep = c
        .streams()
        .replay("orders", Some(10), Some(20), Some(5))
        .await
        .unwrap();
    assert_eq!(rep.data.events[0].sequence, "11");
    assert_eq!(rep.data.events[0].data, json!({"id": 1}));
    assert_eq!(rep.data.next_cursor.as_deref(), Some("11"));
    c.streams()
        .replay("orders", None, None, None)
        .await
        .unwrap();

    let got: Vec<(String, String, Value)> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            assert!(r.url.query().is_none());
            let body = if r.body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&r.body).unwrap()
            };
            (r.method.to_string(), r.url.path().to_string(), body)
        })
        .collect();
    let want = vec![
        (
            "POST",
            "/@portal/data-streams/user",
            json!({"displayName": "Orders", "maxBytes": 1073741824u64}),
        ),
        (
            "POST",
            "/@portal/data-streams/user",
            json!({"displayName": "Orders", "maxBytes": 1073741824u64, "retentionSeconds": 86400}),
        ),
        ("GET", "/@portal/data-streams/user", Value::Null),
        ("GET", "/@portal/data-streams/user/orders", Value::Null),
        ("DELETE", "/@portal/data-streams/user/orders", Value::Null),
        (
            "POST",
            "/@portal/data-streams/user/orders/publish",
            json!({"data": {"id": 1}, "subject": "eu.new"}),
        ),
        (
            "POST",
            "/@portal/data-streams/user/orders/publish",
            json!({"data": 1}),
        ),
        (
            "POST",
            "/@portal/data-streams/user/orders/replay",
            json!({"fromSequence": 10, "untilSequence": 20, "limit": 5}),
        ),
        (
            "POST",
            "/@portal/data-streams/user/orders/replay",
            json!({}),
        ),
    ];
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!((g.0.as_str(), g.1.as_str(), &g.2), (w.0, w.1, &w.2));
    }
}

fn any_request() -> wiremock::matchers::AnyMatcher {
    wiremock::matchers::any()
}

#[tokio::test]
async fn missing_project_slug_is_typed() {
    let c = Client::new(ClientOpts {
        base_url: Some("http://127.0.0.1:1".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c.streams().list().await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}

#[tokio::test]
async fn unavailable_maps_to_kerror() {
    let server = MockServer::start().await;
    Mock::given(any_request())
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "code": "K9O_STREAM_UNAVAILABLE",
            "message": "The Pulse Stream data plane is not yet available in this deployment.",
            "context": {}
        })))
        .mount(&server)
        .await;
    let c = client(&server);
    let e = c
        .streams()
        .publish("orders", &json!(1), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_STREAM_UNAVAILABLE");
    assert_eq!(e.status, Some(503));
    let e = c.streams().subscribe("orders", None).await.err().unwrap();
    assert_eq!(e.code, "K9O_STREAM_UNAVAILABLE");
    assert_eq!(e.status, Some(503));
}

#[tokio::test]
async fn subscribe_resumes_from_last_sequence() {
    let server = MockServer::start().await;
    let route = "/@portal/data-streams/user/orders/subscribe";
    // First connection: no resume point, two events, then the server closes it.
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(sse(format!(
            "retry: 1000\n:ok\n\n{}{}",
            frame("1"),
            frame("2")
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param("fromSequence", "2"))
        .respond_with(sse(format!("retry: 1000\n:ok\n\n{}", frame("3"))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param("fromSequence", "3"))
        .respond_with(sse("retry: 1000\n:ok\n\n".to_string()))
        .mount(&server)
        .await;
    let c = client(&server);
    let mut sub = c
        .streams()
        .subscribe("orders", None)
        .await
        .unwrap()
        .with_reconnect_delay(Duration::from_millis(1));
    let mut seqs = Vec::new();
    while let Some(ev) = sub.next().await {
        seqs.push(ev.unwrap().sequence);
        if seqs.len() == 3 {
            sub.close();
        }
    }
    assert_eq!(seqs, vec!["1", "2", "3"]);
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].url.query(), None);
    assert_eq!(reqs[1].url.query(), Some("fromSequence=2"));
}

#[tokio::test]
async fn subscribe_starts_from_given_sequence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@portal/data-streams/user/orders/subscribe"))
        .and(query_param("fromSequence", "42"))
        .respond_with(sse(frame("43")))
        .mount(&server)
        .await;
    let c = client(&server);
    let mut sub = c.streams().subscribe("orders", Some("42")).await.unwrap();
    let ev = sub.next().await.unwrap().unwrap();
    assert_eq!(ev.sequence, "43");
    assert_eq!(ev.data, json!({"n": "43"}));
}
