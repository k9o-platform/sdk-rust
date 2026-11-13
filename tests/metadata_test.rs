//! `metadata()`: method, path, query and body of every call, `metadata.` prefixing, validation that sends
//! nothing, error decoding (400, 413, 501) and the change feed (reconnect, duplicate drop, `fromSequence`).
//! Fixtures follow the gateway's documented response shapes; no live call is made.

use k9o::{
    Client, ClientOpts, KError, MetadataFilterInput, MetadataIndexColumn, MetadataNearestInput,
    MetadataOp, MetadataSearchInput, MetadataSubscribeOptions, MetadataWhere,
};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(status: u16, body: Value, project: Option<&str>) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: project.map(Into::into),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

fn rows_body(next: Value) -> Value {
    json!({
        "data": [
            {"surface": "collection:posts", "scope": "posts", "recordId": "p1",
             "metadata": {"title": "Quarterly report", "tags": ["pinned", "finance"], "priority": 3, "embedding": "[1,0,0]"}},
            {"surface": "telemetry:collection.metadata_index_declared", "scope": "collection.metadata_index_declared",
             "eventId": "aud_1", "metadata": {"type": "collection.metadata_index_declared", "actorId": "usr_1",
             "occurredAt": "2026-10-26T20:00:00.000Z"}}
        ],
        "meta": {"requestId": "r1", "traceId": "t1", "cursor": {"next": next}}
    })
}

async fn sent(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

fn body_of(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn is_invalid(e: &KError) -> bool {
    e.code == "K9O_INVALID_FIELD_VALUE"
}

#[tokio::test]
async fn filter_sends_prefixed_where_and_decodes_rows_and_cursor() {
    let (server, c) = setup(200, rows_body(json!("cur_2")), Some("portal")).await;
    let r = c
        .metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:posts".into()],
            r#where: Some(vec![
                MetadataWhere::new("priority", MetadataOp::Gte, 2),
                MetadataWhere::new("metadata.title", MetadataOp::StartsWith, "Q"),
                MetadataWhere::new("tags", MetadataOp::Contains, "finance"),
            ]),
            limit: Some(25),
            cursor: Some("cur_1".into()),
        })
        .await
        .unwrap();
    assert_eq!(r.data.len(), 2);
    assert_eq!(r.data[0].record_id.as_deref(), Some("p1"));
    assert_eq!(r.data[0].metadata["priority"], 3);
    assert_eq!(r.data[1].event_id.as_deref(), Some("aud_1"));
    assert!(r.data[1].record_id.is_none());
    assert_eq!(r.meta.cursor.unwrap().next.as_deref(), Some("cur_2"));
    let reqs = sent(&server).await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/@portal/metadata/query");
    assert_eq!(reqs[0].url.query(), None);
    assert_eq!(
        body_of(&reqs[0]),
        json!({
            "surfaces": ["collection:posts"],
            "where": [
                {"field": "metadata.priority", "op": "gte", "value": 2},
                {"field": "metadata.title", "op": "startsWith", "value": "Q"},
                {"field": "metadata.tags", "op": "contains", "value": "finance"}
            ],
            "limit": 25,
            "cursor": "cur_1"
        })
    );
}

#[tokio::test]
async fn filter_last_page_has_no_cursor_and_minimal_body() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    let r = c
        .metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:posts".into(), "collection:notes".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(r.meta.cursor.unwrap().next.is_none());
    assert_eq!(
        body_of(&sent(&server).await[0]),
        json!({"surfaces": ["collection:posts", "collection:notes"]})
    );
}

#[tokio::test]
async fn telemetry_surface_keeps_bare_created_at_and_actor_id() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    c.metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["telemetry:collection.metadata_index_declared".into()],
            r#where: Some(vec![
                MetadataWhere::new("createdAt", MetadataOp::Gt, "2026-10-26T00:00:00Z"),
                MetadataWhere::new("actorId", MetadataOp::Eq, "usr_1"),
                MetadataWhere::new("type", MetadataOp::Eq, "x"),
            ]),
            ..Default::default()
        })
        .await
        .unwrap();
    let b = body_of(&sent(&server).await[0]);
    let fields: Vec<&str> = b["where"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["createdAt", "actorId", "metadata.type"]);
}

#[tokio::test]
async fn created_at_is_prefixed_on_a_collection_surface() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    c.metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:posts".into()],
            r#where: Some(vec![MetadataWhere::new("createdAt", MetadataOp::Gt, "x")]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        body_of(&sent(&server).await[0])["where"][0]["field"],
        "metadata.createdAt"
    );
}

#[tokio::test]
async fn where_clause_without_value_omits_it_and_null_is_kept() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    c.metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:posts".into()],
            r#where: Some(vec![
                MetadataWhere {
                    field: "a".into(),
                    op: MetadataOp::Eq,
                    value: None,
                },
                MetadataWhere::new("b", MetadataOp::Eq, Value::Null),
            ]),
            ..Default::default()
        })
        .await
        .unwrap();
    let b = body_of(&sent(&server).await[0]);
    assert!(b["where"][0].get("value").is_none());
    assert!(b["where"][1]["value"].is_null());
}

#[tokio::test]
async fn filter_validation_sends_nothing() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    let m = c.metadata();
    let base = || MetadataFilterInput {
        surfaces: vec!["collection:posts".into()],
        ..Default::default()
    };
    let e = m.filter(MetadataFilterInput::default()).await.unwrap_err();
    assert!(is_invalid(&e));
    for bad in [
        "posts",
        "files:posts",
        "collection:",
        "collection:1posts",
        "collection:po-sts",
        "storage:-x",
        "telemetry:",
        ":x",
    ] {
        let e = m
            .filter(MetadataFilterInput {
                surfaces: vec![bad.into()],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, "K9O_UNKNOWN_SURFACE", "{bad}");
        assert_eq!(e.context["surface"], bad);
    }
    for limit in [0u32, 1001] {
        let e = m
            .filter(MetadataFilterInput {
                limit: Some(limit),
                ..base()
            })
            .await
            .unwrap_err();
        assert!(is_invalid(&e));
    }
    // A cursor with two surfaces, and an empty cursor.
    let e = m
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:a".into(), "collection:b".into()],
            cursor: Some("c".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(is_invalid(&e));
    assert_eq!(e.context["field"], "cursor");
    let e = m
        .filter(MetadataFilterInput {
            cursor: Some(String::new()),
            ..base()
        })
        .await
        .unwrap_err();
    assert!(is_invalid(&e));
    // An empty column name.
    let e = m
        .filter(MetadataFilterInput {
            r#where: Some(vec![MetadataWhere::new("", MetadataOp::Eq, 1)]),
            ..base()
        })
        .await
        .unwrap_err();
    assert!(is_invalid(&e));
    let e = m
        .filter(MetadataFilterInput {
            r#where: Some(vec![MetadataWhere::new("metadata.", MetadataOp::Eq, 1)]),
            ..base()
        })
        .await
        .unwrap_err();
    assert!(is_invalid(&e));
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn every_operator_serialises_to_its_wire_name() {
    let ops = [
        (MetadataOp::Eq, "eq"),
        (MetadataOp::Neq, "neq"),
        (MetadataOp::Gt, "gt"),
        (MetadataOp::Gte, "gte"),
        (MetadataOp::Lt, "lt"),
        (MetadataOp::Lte, "lte"),
        (MetadataOp::Contains, "contains"),
        (MetadataOp::StartsWith, "startsWith"),
        (MetadataOp::EndsWith, "endsWith"),
        (MetadataOp::In, "in"),
        (MetadataOp::NotIn, "notIn"),
        (MetadataOp::And, "and"),
        (MetadataOp::Or, "or"),
        (MetadataOp::Not, "not"),
    ];
    for (op, name) in ops {
        assert_eq!(serde_json::to_value(op).unwrap(), name);
    }
}

#[tokio::test]
async fn missing_project_context_sends_nothing() {
    let (server, c) = setup(200, rows_body(Value::Null), None).await;
    let e = c
        .metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:posts".into()],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn gateway_errors_decode_with_status_and_context() {
    let cases = [
        (
            413,
            json!({"code": "K9O_QUERY_RESULT_TOO_LARGE", "message": "too many", "context": {"cap": 10000, "surfaces": ["collection:a"]}}),
            "K9O_QUERY_RESULT_TOO_LARGE",
        ),
        (
            501,
            json!({"code": "K9O_NOT_IMPLEMENTED", "message": "storage", "context": {"surface": "storage:docs"}}),
            "K9O_NOT_IMPLEMENTED",
        ),
        (
            400,
            json!({"code": "K9O_METADATA_FIELD_NOT_INDEXED", "message": "no", "context": {"surface": "collection:posts", "field": "metadata.secret"},
                   "recommendation": {"declare": "metadata().index().declare(...)"}}),
            "K9O_METADATA_FIELD_NOT_INDEXED",
        ),
        (
            400,
            json!({"code": "K9O_UNKNOWN_SURFACE", "message": "x", "context": {"surface": "collection:posts"}}),
            "K9O_UNKNOWN_SURFACE",
        ),
        (
            400,
            json!({"code": "K9O_ENCRYPTED_COLUMN_OPERATION_UNSUPPORTED", "message": "x", "context": {"column": "secret", "op": "gt"}}),
            "K9O_ENCRYPTED_COLUMN_OPERATION_UNSUPPORTED",
        ),
        (
            403,
            json!({"code": "K9O_EXTENSION_PERMISSION_DENIED", "message": "x", "context": {"reason": "grant_absent"}}),
            "K9O_EXTENSION_PERMISSION_DENIED",
        ),
    ];
    for (status, body, code) in cases {
        let (_server, c) = setup(status, body, Some("portal")).await;
        let e = c
            .metadata()
            .filter(MetadataFilterInput {
                surfaces: vec!["collection:posts".into()],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(e.status, Some(status));
    }
    let (_s, c) = setup(
        413,
        json!({"code": "K9O_QUERY_RESULT_TOO_LARGE", "message": "m", "context": {"cap": 10000, "surfaces": ["collection:a"]}}),
        Some("portal"),
    )
    .await;
    let e = c
        .metadata()
        .filter(MetadataFilterInput {
            surfaces: vec!["collection:a".into()],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.context["cap"], 10000);
}

#[tokio::test]
async fn set_strips_prefix_and_returns_the_record() {
    let (server, c) = setup(
        200,
        json!({"data": {"id": "p2", "title": "Team offsite 2027", "priority": 9, "secret": "s2"}, "meta": {"requestId": "r"}}),
        Some("portal"),
    )
    .await;
    let mut m = Map::new();
    m.insert("title".into(), json!("Team offsite 2027"));
    m.insert("metadata.priority".into(), json!(9));
    let r = c.metadata().set("posts", "p/2", &m).await.unwrap();
    assert_eq!(r.data["secret"], "s2");
    assert_eq!(r.data["priority"], 9);
    let reqs = sent(&server).await;
    assert_eq!(reqs[0].method.as_str(), "PATCH");
    assert_eq!(
        reqs[0].url.path(),
        "/@portal/metadata/collection/posts/p%2F2"
    );
    assert_eq!(
        body_of(&reqs[0]),
        json!({"metadata": {"title": "Team offsite 2027", "priority": 9}})
    );
}

#[tokio::test]
async fn set_validation_sends_nothing() {
    let (server, c) = setup(200, json!({"data": {}, "meta": {}}), Some("portal")).await;
    let mut one = Map::new();
    one.insert("a".into(), json!(1));
    let m = c.metadata();
    assert!(is_invalid(
        &m.set("bad-name", "p1", &one).await.unwrap_err()
    ));
    assert!(is_invalid(&m.set("posts", "", &one).await.unwrap_err()));
    assert!(is_invalid(
        &m.set("posts", "p1", &Map::new()).await.unwrap_err()
    ));
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn search_prefixes_field_and_sends_limit() {
    let (server, c) = setup(
        200,
        json!({"data": [{"surface": "collection:posts", "scope": "posts", "recordId": "p3",
            "metadata": {"title": "Budget draft", "body": "draft budget, finance review"}}],
            "meta": {"cursor": {"next": null}}}),
        Some("portal"),
    )
    .await;
    let r = c
        .metadata()
        .search(MetadataSearchInput {
            surface: "collection:posts".into(),
            field: "body".into(),
            q: "finance".into(),
            limit: Some(5),
        })
        .await
        .unwrap();
    assert_eq!(r.data[0].record_id.as_deref(), Some("p3"));
    assert!(r.meta.cursor.unwrap().next.is_none());
    let reqs = sent(&server).await;
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/@portal/metadata/search");
    assert_eq!(
        body_of(&reqs[0]),
        json!({"surface": "collection:posts", "field": "metadata.body", "q": "finance", "limit": 5})
    );
}

#[tokio::test]
async fn search_validation_sends_nothing() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    let ok = || MetadataSearchInput {
        surface: "collection:posts".into(),
        field: "body".into(),
        q: "x".into(),
        limit: None,
    };
    let m = c.metadata();
    let e = m
        .search(MetadataSearchInput {
            surface: "telemetry:x".into(),
            ..ok()
        })
        .await
        .unwrap_err();
    assert!(is_invalid(&e));
    let e = m
        .search(MetadataSearchInput {
            surface: "nope".into(),
            ..ok()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_UNKNOWN_SURFACE");
    assert!(is_invalid(
        &m.search(MetadataSearchInput {
            field: String::new(),
            ..ok()
        })
        .await
        .unwrap_err()
    ));
    assert!(is_invalid(
        &m.search(MetadataSearchInput {
            q: String::new(),
            ..ok()
        })
        .await
        .unwrap_err()
    ));
    assert!(is_invalid(
        &m.search(MetadataSearchInput {
            limit: Some(0),
            ..ok()
        })
        .await
        .unwrap_err()
    ));
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn nearest_sends_body_prefixes_column_and_where() {
    let (server, c) = setup(
        200,
        json!({"data": [{"surface":"collection:posts","scope":"posts","recordId":"p1","metadata":{"title":"Quarterly report","embedding":"[1,0,0]"}}],
               "meta": {"cursor": {"next": null}}}),
        Some("portal"),
    )
    .await;
    let r = c
        .metadata()
        .vectors()
        .nearest(MetadataNearestInput {
            surface: "collection:posts".into(),
            column: "embedding".into(),
            embedding: vec![1.0, 0.0, 0.0],
            k: Some(3),
            metric: Some("euclidean".into()),
            r#where: Some(vec![MetadataWhere::new("priority", MetadataOp::Gt, 1)]),
        })
        .await
        .unwrap();
    assert_eq!(r.data[0].record_id.as_deref(), Some("p1"));
    let reqs = sent(&server).await;
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(reqs[0].url.path(), "/@portal/metadata/vectors/nearest");
    assert_eq!(
        body_of(&reqs[0]),
        json!({
            "surface": "collection:posts",
            "column": "metadata.embedding",
            "embedding": [1.0, 0.0, 0.0],
            "k": 3,
            "metric": "euclidean",
            "where": [{"field": "metadata.priority", "op": "gt", "value": 1}]
        })
    );
}

#[tokio::test]
async fn nearest_minimal_body_and_validation() {
    let (server, c) = setup(200, rows_body(Value::Null), Some("portal")).await;
    let ok = || MetadataNearestInput {
        surface: "collection:posts".into(),
        column: "metadata.embedding".into(),
        embedding: vec![0.5],
        ..Default::default()
    };
    let v = c.metadata().vectors();
    v.nearest(ok()).await.unwrap();
    assert_eq!(
        body_of(&sent(&server).await[0]),
        json!({"surface": "collection:posts", "column": "metadata.embedding", "embedding": [0.5]})
    );
    let before = sent(&server).await.len();
    for bad in [
        MetadataNearestInput {
            embedding: vec![],
            ..ok()
        },
        MetadataNearestInput {
            embedding: vec![1.0, f64::NAN],
            ..ok()
        },
        MetadataNearestInput {
            embedding: vec![f64::INFINITY],
            ..ok()
        },
        MetadataNearestInput { k: Some(0), ..ok() },
        MetadataNearestInput {
            k: Some(1001),
            ..ok()
        },
        MetadataNearestInput {
            metric: Some("manhattan".into()),
            ..ok()
        },
        MetadataNearestInput {
            surface: "storage:docs".into(),
            ..ok()
        },
        MetadataNearestInput {
            column: String::new(),
            ..ok()
        },
    ] {
        assert!(is_invalid(&v.nearest(bad).await.unwrap_err()));
    }
    let e = v
        .nearest(MetadataNearestInput {
            surface: "x".into(),
            ..ok()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_UNKNOWN_SURFACE");
    assert_eq!(sent(&server).await.len(), before);
}

#[tokio::test]
async fn index_get_and_declare() {
    let (server, c) = setup(
        200,
        json!({"data": {"collection": "posts", "columns": [{"column": "body", "indexed": false}, {"column": "tags", "indexed": true}]},
               "meta": {"requestId": "r"}}),
        Some("portal"),
    )
    .await;
    let idx = c.metadata().index().get("posts").await.unwrap().data;
    assert_eq!(idx.collection, "posts");
    assert_eq!(idx.columns.len(), 2);
    assert!(idx.columns[1].indexed);
    c.metadata()
        .index()
        .declare(
            "posts",
            &[
                MetadataIndexColumn::new("title"),
                MetadataIndexColumn::new("tags").indexed(true),
            ],
        )
        .await
        .unwrap();
    c.metadata().index().declare("posts", &[]).await.unwrap();
    let reqs = sent(&server).await;
    assert_eq!(reqs[0].method.as_str(), "GET");
    assert_eq!(reqs[0].url.path(), "/@portal/metadata/index/posts");
    assert_eq!(reqs[1].method.as_str(), "PUT");
    assert_eq!(reqs[1].url.path(), "/@portal/metadata/index/posts");
    assert_eq!(
        body_of(&reqs[1]),
        json!({"columns": [{"column": "title"}, {"column": "tags", "indexed": true}]})
    );
    assert_eq!(body_of(&reqs[2]), json!({"columns": []}));
}

#[tokio::test]
async fn index_validation_sends_nothing() {
    let (server, c) = setup(200, json!({"data": {}, "meta": {}}), Some("portal")).await;
    let i = c.metadata().index();
    assert!(is_invalid(&i.get("1bad").await.unwrap_err()));
    let many: Vec<MetadataIndexColumn> = (0..65)
        .map(|n| MetadataIndexColumn::new(format!("c{n}")))
        .collect();
    assert!(is_invalid(&i.declare("posts", &many).await.unwrap_err()));
    assert!(is_invalid(
        &i.declare("posts", &[MetadataIndexColumn::new("id")])
            .await
            .unwrap_err()
    ));
    assert!(is_invalid(
        &i.declare("posts", &[MetadataIndexColumn::new("bad-name")])
            .await
            .unwrap_err()
    ));
    assert!(is_invalid(
        &i.declare(
            "posts",
            &[MetadataIndexColumn::new("a"), MetadataIndexColumn::new("a")]
        )
        .await
        .unwrap_err()
    ));
    assert!(is_invalid(&i.declare("bad-c", &[]).await.unwrap_err()));
    // Exactly 64 columns is allowed.
    let sixty_four: Vec<MetadataIndexColumn> = (0..64)
        .map(|n| MetadataIndexColumn::new(format!("c{n}")))
        .collect();
    i.declare("posts", &sixty_four).await.unwrap();
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn index_not_found_decodes() {
    let (_s, c) = setup(
        404,
        json!({"code": "K9O_COLLECTION_NOT_FOUND", "message": "x"}),
        Some("portal"),
    )
    .await;
    let e = c.metadata().index().get("nope").await.unwrap_err();
    assert_eq!(e.code, "K9O_COLLECTION_NOT_FOUND");
    assert_eq!(e.status, Some(404));
}

// ---- change feed ----

enum Script {
    Reply(u16, String),
    Hang(String),
}

struct Fake {
    uri: String,
    /// Request target (path and query) and Last-Event-ID of each request.
    seen: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

async fn fake(scripts: Vec<Script>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        let mut scripts = scripts.into_iter();
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&got).to_string();
            let target = head
                .lines()
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();
            let last = head
                .lines()
                .find(|l| l.to_lowercase().starts_with("last-event-id:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            log.lock().unwrap().push((target, last));
            let s = scripts.next().unwrap_or(Script::Hang(String::new()));
            tokio::spawn(async move {
                match s {
                    Script::Reply(status, body) => {
                        let ct = if status == 200 {
                            "text/event-stream"
                        } else {
                            "application/json"
                        };
                        let r = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\nconnection: close\r\n\r\n{body}"
                        );
                        let _ = sock.write_all(r.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Script::Hang(body) => {
                        let r = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                        );
                        let _ = sock.write_all(r.as_bytes()).await;
                        let mut b = [0u8; 16];
                        let _ = sock.read(&mut b).await;
                    }
                }
            });
        }
    });
    Fake { uri, seen }
}

fn feed_client(f: &Fake) -> Client {
    Client::new(ClientOpts {
        base_url: Some(f.uri.clone()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap()
}

/// A frame as the gateway sends it: the id is only in the `id:` line, and the payload is `{ collection, id }`.
fn frame(id: &str, event: &str, rec: &str) -> String {
    format!(
        "id: {id}\nevent: {event}\ndata: {{\"topic\":\"collections.posts.{event}\",\"eventType\":\"{event}\",\"payload\":{{\"collection\":\"posts\",\"id\":\"{rec}\"}},\"occurredAt\":\"2026-10-27T10:20:38.969Z\"}}\n\n"
    )
}

fn fast() -> MetadataSubscribeOptions {
    MetadataSubscribeOptions::new().reconnect_delay(Duration::from_millis(10))
}

async fn next_ev(sub: &mut k9o::Subscription) -> k9o::RealtimeEvent {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .expect("closed")
        .expect("error")
}

#[tokio::test]
async fn subscribe_reads_id_only_frames_and_sends_surface_and_from_sequence_once() {
    let first = format!(
        "retry: 1000\n:ok\n\n{}{}",
        frame("c1", "row_inserted", "p1"),
        frame("c2", "row_updated", "p2")
    );
    // The resend repeats c2 (lookback), then one new event.
    let second = format!(
        "{}{}",
        frame("c2", "row_updated", "p2"),
        frame("c3", "row_deleted", "p3")
    );
    let f = fake(vec![Script::Reply(200, first), Script::Hang(second)]).await;
    let c = feed_client(&f);
    let mut sub = c
        .metadata()
        .subscribe("collection:posts", fast().from_sequence(7))
        .await
        .unwrap();
    let a = next_ev(&mut sub).await;
    assert_eq!(a.event_type, "row_inserted");
    assert_eq!(a.event_id, "c1");
    let change = k9o::MetadataChange::from_payload(&a.payload).unwrap();
    assert_eq!(
        (change.collection.as_str(), change.id.as_str()),
        ("posts", "p1")
    );
    assert_eq!(next_ev(&mut sub).await.event_id, "c2");
    let d = next_ev(&mut sub).await;
    assert_eq!(
        (d.event_type.as_str(), d.event_id.as_str()),
        ("row_deleted", "c3")
    );
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0],
        (
            "/@portal/metadata/subscribe?surface=collection%3Aposts&fromSequence=7".to_string(),
            None
        )
    );
    // Reconnect: Last-Event-ID, and no fromSequence.
    assert_eq!(
        seen[1],
        (
            "/@portal/metadata/subscribe?surface=collection%3Aposts".to_string(),
            Some("c2".to_string())
        )
    );
}

#[tokio::test]
async fn subscribe_resync_calls_handler_and_drops_from_sequence() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-evicted\",\"action\":\"reread-source\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, resync),
        Script::Hang(frame("c9", "row_updated", "p9")),
    ])
    .await;
    let c = feed_client(&f);
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = calls.clone();
    let opts = fast().from_sequence(3).on_resync(move |r| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(r.reason);
        }
    });
    let mut sub = c
        .metadata()
        .subscribe("collection:posts", opts)
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "c9");
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec!["cursor-evicted".to_string()]
    );
    let seen = f.seen.lock().unwrap().clone();
    assert!(seen[0].0.ends_with("fromSequence=3"));
    assert!(!seen[1].0.contains("fromSequence"));
    assert_eq!(seen[1].1, None);
}

#[tokio::test]
async fn subscribe_opening_resync_is_informational_and_the_events_follow() {
    // The gateway's fresh-connection opening: retry, :ok, a head resync, then events.
    let body = format!(
        "retry: 1000\n:ok\n\nevent: resync\ndata: {{\"reason\":\"cursor-evicted\",\"head\":75,\"action\":\"reread-source\"}}\n\n{}",
        frame("c1", "row_inserted", "p1")
    );
    let f = fake(vec![Script::Hang(body)]).await;
    let c = feed_client(&f);
    let calls: Arc<Mutex<u32>> = Arc::default();
    let n = calls.clone();
    let opts = fast().on_resync(move |_| {
        let n = n.clone();
        async move {
            *n.lock().unwrap() += 1;
        }
    });
    let mut sub = c
        .metadata()
        .subscribe("collection:posts", opts)
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "c1");
    assert_eq!(*calls.lock().unwrap(), 0);
    assert_eq!(f.seen.lock().unwrap().len(), 1, "no reconnect");
}

#[tokio::test]
async fn subscribe_without_from_sequence_sends_only_surface() {
    let f = fake(vec![Script::Hang(frame("c1", "row_inserted", "p1"))]).await;
    let c = feed_client(&f);
    let mut sub = c
        .metadata()
        .subscribe("collection:posts", fast())
        .await
        .unwrap();
    next_ev(&mut sub).await;
    assert_eq!(
        f.seen.lock().unwrap()[0].0,
        "/@portal/metadata/subscribe?surface=collection%3Aposts"
    );
}

#[tokio::test]
async fn subscribe_rejects_non_collection_surfaces_without_connecting() {
    let f = fake(vec![]).await;
    let c = feed_client(&f);
    let m = c.metadata();
    let e = m
        .subscribe("telemetry:x", MetadataSubscribeOptions::new())
        .await
        .err()
        .unwrap();
    assert!(is_invalid(&e));
    let e = m
        .subscribe("posts", MetadataSubscribeOptions::new())
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, "K9O_UNKNOWN_SURFACE");
    assert!(f.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn subscribe_client_error_is_returned_once() {
    let body = "{\"code\":\"K9O_ACCESS_DENIED\",\"message\":\"no\"}".to_string();
    let f = fake(vec![Script::Reply(403, body)]).await;
    let c = feed_client(&f);
    let e = c
        .metadata()
        .subscribe("collection:posts", fast())
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, "K9O_ACCESS_DENIED");
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}
