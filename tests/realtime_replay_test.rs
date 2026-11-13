//! `realtime().replay`: request path, method and body, validation, decoding and errors, over a
//! scripted local server. Each call takes the next script.

use k9o::{Client, ClientOpts, ReplayOptions};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

enum Script {
    /// Answer with a status, a content type and a body, then close.
    Reply(u16, &'static str, String),
    /// SSE headers and `body`, then stay open and silent.
    Hang(String),
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    #[allow(dead_code)]
    last_event_id: Option<String>,
    body: String,
}

struct Fake {
    uri: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn fake(scripts: Vec<Script>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        let mut scripts = scripts.into_iter();
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = Vec::new();
            let (head, body) = loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    got.extend_from_slice(&buf[..n]);
                }
                if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..pos]).to_string();
                    let want = head
                        .lines()
                        .find(|l| l.to_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split_once(':').unwrap().1.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if got.len() >= pos + 4 + want {
                        let body =
                            String::from_utf8_lossy(&got[pos + 4..pos + 4 + want]).to_string();
                        break (head, body);
                    }
                }
                if n == 0 {
                    break (String::from_utf8_lossy(&got).to_string(), String::new());
                }
            };
            let first = head.lines().next().unwrap_or("").to_string();
            let mut parts = first.split(' ');
            let method = parts.next().unwrap_or("").to_string();
            let target = parts.next().unwrap_or("").to_string();
            let last = head
                .lines()
                .find(|l| l.to_lowercase().starts_with("last-event-id:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            log.lock().unwrap().push(Seen {
                method,
                target,
                last_event_id: last,
                body,
            });
            let s = scripts.next().unwrap_or(Script::Hang(String::new()));
            tokio::spawn(async move {
                match s {
                    Script::Reply(status, ct, body) => {
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

fn client(f: &Fake) -> Client {
    Client::new(ClientOpts {
        base_url: Some(f.uri.clone()),
        project_slug: Some("portal".into()),
        retry_config: Some(k9o::RetryConfig {
            max_retries: 0,
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap()
}

fn url_of(target: &str) -> reqwest::Url {
    reqwest::Url::parse(&format!("http://x{target}")).unwrap()
}

fn seen_body(f: &Fake, i: usize) -> Value {
    serde_json::from_str(&f.seen.lock().unwrap()[i].body).unwrap()
}

#[tokio::test]
async fn replay_posts_the_topic_and_options_and_decodes_events() {
    let f = fake(vec![Script::Reply(
        200,
        "application/json",
        json!({"data": {"topic": "audit.project.*", "events": [
            {"topic": "audit.project.t.b", "eventType": "t.b", "occurredAt": "2026-11-13T10:00:00Z",
             "eventId": "fe-1", "payload": {"auditEventId": "evt_B"}},
            {"eventType": "t.c", "payload": {"auditEventId": "evt_C"}}
        ], "nextCursor": 7, "hasMore": true}, "meta": {}})
        .to_string(),
    )])
    .await;
    let c = client(&f);
    let r = c
        .realtime()
        .replay(
            "audit.project.*",
            ReplayOptions {
                from_sequence: Some(5),
                until_sequence: Some(9),
                limit: Some(100),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .data;
    assert_eq!(r.events.len(), 2);
    assert_eq!(r.events[0].topic, "audit.project.t.b");
    assert_eq!(r.events[0].event_type, "t.b");
    assert_eq!(r.events[0].event_id, "fe-1");
    assert_eq!(r.events[0].occurred_at, "2026-11-13T10:00:00Z");
    assert_eq!(r.events[0].payload["auditEventId"], "evt_B");
    // An event without a topic takes the answer's topic.
    assert_eq!(r.events[1].topic, "audit.project.*");
    assert_eq!(r.next_cursor.as_deref(), Some("7"));
    assert!(r.has_more);
    assert_eq!(r.resync, None);
    let s = f.seen.lock().unwrap()[0].clone();
    assert_eq!(
        (s.method.as_str(), url_of(&s.target).path()),
        ("POST", "/@portal/realtime/replay")
    );
    assert_eq!(
        seen_body(&f, 0),
        json!({"topic": "audit.project.*", "fromSequence": 5, "untilSequence": 9, "limit": 100})
    );
}

#[tokio::test]
async fn replay_sends_only_the_topic_by_default_and_after_audit_event_id() {
    let f = fake(vec![
        Script::Reply(
            200,
            "application/json",
            json!({"data": {"topic": "collections.tasks.*", "events": [], "nextCursor": null,
                "hasMore": false, "resync": "reread-source"}, "meta": {}})
            .to_string(),
        ),
        Script::Reply(
            200,
            "application/json",
            json!({"data": {"events": [], "hasMore": false}, "meta": {}}).to_string(),
        ),
    ])
    .await;
    let c = client(&f);
    let r = c
        .realtime()
        .replay("collections.tasks.*", ReplayOptions::default())
        .await
        .unwrap()
        .data;
    assert!(r.events.is_empty() && !r.has_more && r.next_cursor.is_none());
    assert_eq!(r.resync.as_deref(), Some("reread-source"));
    assert_eq!(seen_body(&f, 0), json!({"topic": "collections.tasks.*"}));
    c.realtime()
        .replay(
            "audit.project.*",
            ReplayOptions {
                after_audit_event_id: Some("evt_A1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        seen_body(&f, 1),
        json!({"topic": "audit.project.*", "afterAuditEventId": "evt_A1"})
    );
}

#[tokio::test]
async fn replay_validates_before_sending() {
    let f = fake(vec![Script::Reply(
        200,
        "application/json",
        json!({"data": {"events": [], "hasMore": false}, "meta": {}}).to_string(),
    )])
    .await;
    let c = client(&f);
    let rt = c.realtime();
    let long = "t".repeat(201);
    let opt = |g: fn(&mut ReplayOptions)| {
        let mut o = ReplayOptions::default();
        g(&mut o);
        o
    };
    let bad = [
        rt.replay("", ReplayOptions::default()).await,
        rt.replay(&long, ReplayOptions::default()).await,
        rt.replay("a.b", opt(|o| o.limit = Some(0))).await,
        rt.replay("a.b", opt(|o| o.limit = Some(10001))).await,
        rt.replay("a.b", opt(|o| o.after_audit_event_id = Some("nope".into())))
            .await,
        rt.replay(
            "audit.project.*",
            opt(|o| {
                o.after_audit_event_id = Some("evt_A".into());
                o.from_sequence = Some(1);
            }),
        )
        .await,
    ];
    for r in bad {
        assert_eq!(r.unwrap_err().code, "K9O_INVALID_FIELD_VALUE");
    }
    // Nothing above reached the server; the maximum limit is accepted and sent.
    assert!(f.seen.lock().unwrap().is_empty());
    rt.replay("a.b", opt(|o| o.limit = Some(10000)))
        .await
        .unwrap();
    assert_eq!(seen_body(&f, 0), json!({"topic": "a.b", "limit": 10000}));
}

#[tokio::test]
async fn replay_without_a_project_is_refused() {
    let c = Client::new(ClientOpts {
        base_url: Some("http://127.0.0.1:1".into()),
        ..Default::default()
    })
    .unwrap();
    assert!(c
        .realtime()
        .replay("a.b", ReplayOptions::default())
        .await
        .is_err());
}

#[tokio::test]
async fn replay_surfaces_gateway_errors() {
    let f = fake(vec![
        Script::Reply(
            403,
            "application/json",
            "{\"code\":\"K9O_RBAC_DENY\",\"message\":\"no\"}".into(),
        ),
        Script::Reply(
            400,
            "application/json",
            "{\"code\":\"K9O_INVALID_CURSOR\",\"message\":\"not in the log\"}".into(),
        ),
    ])
    .await;
    let c = client(&f);
    let e = c
        .realtime()
        .replay("audit.project.*", ReplayOptions::default())
        .await
        .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_RBAC_DENY", Some(403)));
    let e = c
        .realtime()
        .replay(
            "audit.project.*",
            ReplayOptions {
                after_audit_event_id: Some("evt_Z".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("K9O_INVALID_CURSOR", Some(400))
    );
}
