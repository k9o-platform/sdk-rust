//! Live audit tail (`audit().tail`), `audit().replay` and the rollup cell feed, over a scripted local
//! server so connections can stay open, close or go quiet on demand. Each connection and each replay
//! call takes the next script.

use k9o::{
    AuditReplayOptions, AuditTailOptions, Client, ClientOpts, RollupSubscribeOptions,
    SubscribeOptions,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

/// A real-shape frame: the SSE `id` (`sse-<audit id>`) and the frame `eventId` (`fe-<audit id>`) are
/// both different from the `auditEventId` inside `payload`.
fn frame(id: &str, kind: &str) -> String {
    format!(
        "id: sse-{id}\nevent: {kind}\ndata: {{\"topic\":\"audit.project.{kind}\",\"eventType\":\"{kind}\",\"payload\":{{\"auditEventId\":\"{id}\",\"type\":\"{kind}\",\"actorType\":\"user\",\"occurredAt\":\"2026-10-27T10:20:38.969Z\",\"outcome\":null}},\"occurredAt\":\"2026-10-27T10:20:38.969Z\",\"eventId\":\"fe-{id}\"}}\n\n"
    )
}

fn fast() -> AuditTailOptions {
    AuditTailOptions::new().reconnect_delay(Duration::from_millis(10))
}

async fn next_ev(t: &mut k9o::AuditTail) -> k9o::AuditTailEvent {
    tokio::time::timeout(Duration::from_secs(5), t.next())
        .await
        .expect("timed out")
        .expect("closed")
        .expect("error")
}

fn topic_of(target: &str) -> String {
    let url = url_of(target);
    url.query_pairs()
        .find(|(k, _)| k == "topic")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn url_of(target: &str) -> reqwest::Url {
    reqwest::Url::parse(&format!("http://x{target}")).unwrap()
}

#[tokio::test]
async fn tail_subscribes_to_the_project_audit_topic_and_decodes_the_five_fields() {
    let body = format!(
        ":ok\n\n{}:hb\n\n",
        frame("evt_A", "data.write.access.insert")
    );
    let f = fake(vec![Script::Hang(body)]).await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let ev = next_ev(&mut t).await;
    assert_eq!(ev.audit_event_id, "evt_A");
    assert_eq!(ev.event_type, "data.write.access.insert");
    assert_eq!(ev.actor_type, "user");
    assert_eq!(ev.occurred_at, "2026-10-27T10:20:38.969Z");
    assert_eq!(ev.outcome, None);
    assert_eq!(ev.event_id.as_deref(), Some("fe-evt_A"));
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        url_of(&seen[0].target).path(),
        "/@portal/realtime/subscribe"
    );
    assert_eq!(topic_of(&seen[0].target), "audit.project.*");
    assert_eq!(seen[0].last_event_id, None);
}

#[tokio::test]
async fn one_type_subscribes_to_that_topic() {
    let f = fake(vec![Script::Hang(frame("evt_A", "gate.signin"))]).await;
    let c = client(&f);
    let mut t = c.audit().tail(fast().types(["gate.signin"])).await.unwrap();
    assert_eq!(next_ev(&mut t).await.event_type, "gate.signin");
    assert_eq!(
        topic_of(&f.seen.lock().unwrap()[0].target),
        "audit.project.gate.signin"
    );
}

#[tokio::test]
async fn several_types_subscribe_to_all_and_drop_the_rest() {
    let body = format!(
        "{}{}{}",
        frame("evt_A", "a.one"),
        frame("evt_B", "z.other"),
        frame("evt_C", "b.two")
    );
    let f = fake(vec![Script::Hang(body)]).await;
    let c = client(&f);
    let mut t = c
        .audit()
        .tail(fast().types(["a.one", "b.two"]))
        .await
        .unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_C");
    assert_eq!(
        topic_of(&f.seen.lock().unwrap()[0].target),
        "audit.project.*"
    );
}

#[tokio::test]
async fn a_bad_type_is_refused_before_connecting() {
    let f = fake(vec![]).await;
    let c = client(&f);
    for bad in ["", "a b", "a*", "x?y=1"] {
        let e = c.audit().tail(fast().types([bad])).await.err().unwrap();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    }
    assert!(f.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconnects_with_last_event_id_and_drops_duplicates() {
    let first = format!("{}{}:hb\n\n", frame("evt_A", "t.a"), frame("evt_B", "t.b"));
    let second = format!(
        "{}{}{}",
        frame("evt_A", "t.a"),
        frame("evt_B", "t.b"),
        frame("evt_C", "t.c")
    );
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", first),
        Script::Hang(second),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let ids = [
        next_ev(&mut t).await.audit_event_id,
        next_ev(&mut t).await.audit_event_id,
        next_ev(&mut t).await.audit_event_id,
    ];
    assert_eq!(ids, ["evt_A", "evt_B", "evt_C"]);
    let last: Vec<Option<String>> = f
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.last_event_id.clone())
        .collect();
    assert_eq!(last, vec![None, Some("sse-evt_B".to_string())]);
}

#[tokio::test]
async fn resync_with_replay_ledger_catches_up_through_the_replay_endpoint_then_continues() {
    let first = frame("evt_A", "t.a");
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n"
            .to_string();
    // The replay returns evt_A again (already delivered, dropped), then two new events.
    let replay = json!({"data": {"topic": "audit.project.*", "events": [
        {"sequence": "10", "topic": "audit.project.t.a", "eventType": "t.a", "occurredAt": "o",
         "payload": {"auditEventId": "evt_A", "type": "t.a", "actorType": "user", "occurredAt": "o", "outcome": "success"}},
        {"sequence": "11", "topic": "audit.project.warden.checkpoint", "eventType": "warden.checkpoint", "occurredAt": "o2",
         "payload": {"auditEventId": "evt_B", "type": "warden.checkpoint", "actorType": "system", "occurredAt": "o2", "outcome": null}},
        {"sequence": "12", "topic": "audit.project.t.c", "eventType": "t.c", "occurredAt": "o3",
         "payload": {"auditEventId": "evt_C", "type": "t.c", "actorType": "user", "occurredAt": "o3", "outcome": "denied"}}
    ], "nextCursor": null, "hasMore": false}, "meta": {}})
    .to_string();
    // The fresh connection repeats evt_C (dropped) and adds evt_D.
    let third = format!("{}{}", frame("evt_C", "t.c"), frame("evt_D", "t.d"));
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", first),
        Script::Reply(200, "text/event-stream", resync),
        Script::Hang(third),
        Script::Reply(200, "application/json", replay),
    ])
    .await;
    let c = client(&f);
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = calls.clone();
    let opts = fast().on_resync(move |r| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push((r.reason, r.action));
        }
    });
    let mut t = c.audit().tail(opts).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(next_ev(&mut t).await.audit_event_id);
    }
    assert_eq!(ids, ["evt_A", "evt_B", "evt_C", "evt_D"]);
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![("cursor-unknown".to_string(), "replay-ledger".to_string())]
    );
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    // 1: first connect; 2: reconnect with the last id (server answers resync);
    // 3: a fresh connection without Last-Event-ID; 4: the replay call, made once it is open.
    assert_eq!(seen[0].last_event_id, None);
    assert_eq!(seen[1].last_event_id.as_deref(), Some("sse-evt_A"));
    assert_eq!(seen[2].method, "GET");
    assert_eq!(seen[2].last_event_id, None);
    assert_eq!(seen[3].method, "POST");
    assert_eq!(url_of(&seen[3].target).path(), "/@portal/realtime/replay");
    let body: Value = serde_json::from_str(&seen[3].body).unwrap();
    assert_eq!(
        body,
        json!({"topic": "audit.project.*", "afterAuditEventId": "evt_A", "limit": 1000})
    );
}

#[tokio::test]
async fn resync_before_any_event_has_nothing_to_replay() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", resync),
        Script::Hang(frame("evt_A", "t.a")),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.method == "GET"));
}

#[tokio::test]
async fn a_failed_replay_ends_the_tail_with_the_error() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", resync),
        Script::Hang(String::new()),
        Script::Reply(
            400,
            "application/json",
            "{\"code\":\"K9O_INVALID_CURSOR\",\"message\":\"not in the log\"}".into(),
        ),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    next_ev(&mut t).await;
    let e = t.next().await.unwrap().unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("K9O_INVALID_CURSOR", Some(400))
    );
    assert!(t.next().await.is_none());
}

#[tokio::test]
async fn idle_connection_is_abandoned_and_reconnected() {
    let f = fake(vec![
        Script::Hang(frame("evt_A", "t.a")),
        Script::Hang(frame("evt_B", "t.b")),
    ])
    .await;
    let c = client(&f);
    let mut t = c
        .audit()
        .tail(fast().idle_timeout(Duration::from_millis(150)))
        .await
        .unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_B");
    let last: Vec<Option<String>> = f
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.last_event_id.clone())
        .collect();
    assert_eq!(last, vec![None, Some("sse-evt_A".to_string())]);
}

#[tokio::test]
async fn forbidden_on_open_is_returned_once() {
    let f = fake(vec![Script::Reply(
        403,
        "application/json",
        "{\"code\":\"K9O_RBAC_DENY\",\"message\":\"audit access needed\"}".into(),
    )])
    .await;
    let c = client(&f);
    let e = c.audit().tail(fast()).await.err().unwrap();
    assert_eq!((e.code.as_str(), e.status), ("K9O_RBAC_DENY", Some(403)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn forbidden_on_reconnect_ends_the_tail() {
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(
            403,
            "application/json",
            "{\"code\":\"K9O_RBAC_DENY\",\"message\":\"role removed\"}".into(),
        ),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    next_ev(&mut t).await;
    let e = t.next().await.unwrap().unwrap_err();
    assert_eq!(e.code, "K9O_RBAC_DENY");
    assert!(t.next().await.is_none());
}

#[tokio::test]
async fn close_stops_the_tail() {
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Hang(String::new()),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    next_ev(&mut t).await;
    t.close();
    assert!(t.next().await.is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn replay_sends_the_cursor_and_decodes_events() {
    let f = fake(vec![Script::Reply(
        200,
        "application/json",
        json!({"data": {"topic": "audit.project.*", "events": [
            {"sequence": "5", "topic": "audit.project.t.b", "eventType": "t.b", "occurredAt": "o",
             "payload": {"auditEventId": "evt_B", "type": "t.b", "actorType": "user", "occurredAt": "o", "outcome": null}}
        ], "nextCursor": "5", "hasMore": false}, "meta": {}})
        .to_string(),
    )])
    .await;
    let c = client(&f);
    let r = c
        .audit()
        .replay(AuditReplayOptions {
            topic: "audit.project.*".into(),
            after_audit_event_id: Some("evt_A".into()),
            limit: Some(100),
            ..Default::default()
        })
        .await
        .unwrap()
        .data;
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].audit_event_id, "evt_B");
    assert_eq!(r.events[0].sequence.as_deref(), Some("5"));
    assert_eq!(r.events[0].outcome, None);
    assert_eq!(r.events[0].event_type, "t.b");
    assert_eq!(r.next_cursor.as_deref(), Some("5"));
    assert!(!r.has_more);
    let s = f.seen.lock().unwrap()[0].clone();
    assert_eq!(
        (s.method.as_str(), url_of(&s.target).path()),
        ("POST", "/@portal/realtime/replay")
    );
    let body: Value = serde_json::from_str(&s.body).unwrap();
    assert_eq!(
        body,
        json!({"topic": "audit.project.*", "afterAuditEventId": "evt_A", "limit": 100})
    );
}

#[tokio::test]
async fn replay_from_sequence_and_checks() {
    let f = fake(vec![Script::Reply(
        200,
        "application/json",
        json!({"data": {"events": [], "hasMore": true}, "meta": {}}).to_string(),
    )])
    .await;
    let c = client(&f);
    let r = c
        .audit()
        .replay(AuditReplayOptions {
            topic: "audit.project.gate.signin".into(),
            from_sequence: Some(40),
            until_sequence: Some(50),
            ..Default::default()
        })
        .await
        .unwrap()
        .data;
    assert!(r.has_more && r.events.is_empty());
    let body: Value = serde_json::from_str(&f.seen.lock().unwrap()[0].body).unwrap();
    assert_eq!(
        body,
        json!({"topic": "audit.project.gate.signin", "fromSequence": 40, "untilSequence": 50, "limit": 1000})
    );

    let a = c.audit();
    let mk = |f: fn(&mut AuditReplayOptions)| {
        let mut o = AuditReplayOptions {
            topic: "audit.project.*".into(),
            ..Default::default()
        };
        f(&mut o);
        o
    };
    assert!(a.replay(mk(|o| o.topic.clear())).await.is_err());
    assert!(a
        .replay(mk(|o| o.after_audit_event_id = Some("nope".into())))
        .await
        .is_err());
    assert!(a.replay(mk(|o| o.limit = Some(0))).await.is_err());
    assert!(a.replay(mk(|o| o.limit = Some(10001))).await.is_err());
    assert!(a
        .replay(mk(|o| {
            o.after_audit_event_id = Some("evt_A".into());
            o.from_sequence = Some(1);
        }))
        .await
        .is_err());
    assert_eq!(
        f.seen.lock().unwrap().len(),
        1,
        "nothing was sent for the bad inputs"
    );
}

#[tokio::test]
async fn replay_invalid_cursor_is_a_typed_error() {
    let f = fake(vec![Script::Reply(
        400,
        "application/json",
        "{\"code\":\"K9O_INVALID_CURSOR\",\"message\":\"not in the log\"}".into(),
    )])
    .await;
    let c = client(&f);
    let e = c
        .audit()
        .replay(AuditReplayOptions {
            topic: "audit.project.*".into(),
            after_audit_event_id: Some("evt_zzz".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("K9O_INVALID_CURSOR", Some(400))
    );
}

// ---- rollup cell feed -----------------------------------------------------------------------

/// A real-shape cell frame: SSE `id` is the resume cursor (`cur-<id>`), `eventId` is `<id>`.
fn cell(id: &str, n: u32) -> String {
    format!(
        "id: cur-{id}\nevent: cell\ndata: {{\"topic\":\"rollups.r\",\"eventType\":\"cell\",\"payload\":{{\"cellKey\":\"k{n}\",\"bucket\":\"2026-10-27T10:00:00Z\",\"dims\":{{\"region\":\"de\"}},\"final\":true,\"measures\":{{\"n\":{n}}}}},\"occurredAt\":\"2026-10-27T10:00:05Z\",\"eventId\":\"{id}\"}}\n\n"
    )
}

fn suppressed(id: &str) -> String {
    format!(
        "id: cur-{id}\nevent: cell\ndata: {{\"topic\":\"rollups.r\",\"eventType\":\"cell\",\"payload\":{{\"cellKey\":\"ks\",\"bucket\":\"2026-10-27T10:00:00Z\",\"dims\":{{}},\"final\":true,\"status\":\"suppressed\",\"reason\":\"cohort_below_k\"}},\"occurredAt\":\"2026-10-27T10:00:05Z\",\"eventId\":\"{id}\"}}\n\n"
    )
}

#[tokio::test]
async fn rollup_subscribe_delivers_cells_resumes_and_drops_duplicates() {
    let first = format!("{}{}", cell("evt_1", 1), cell("evt_2", 2));
    let second = format!("{}{}", cell("evt_2", 2), cell("evt_3", 3));
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", first),
        Script::Hang(second),
    ])
    .await;
    let c = client(&f);
    let opts = RollupSubscribeOptions {
        from_sequence: Some(7),
        feed: SubscribeOptions::new().reconnect_delay(Duration::from_millis(10)),
    };
    let mut sub = c.rollups().subscribe("orders_hourly", opts).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..3 {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(ev.is_final && !ev.is_suppressed());
        assert_eq!(ev.topic, "rollups.r");
        got.push((
            ev.event_id.clone(),
            ev.measures.as_ref().unwrap()["n"].as_u64().unwrap(),
        ));
    }
    assert_eq!(
        got,
        vec![
            ("evt_1".to_string(), 1),
            ("evt_2".to_string(), 2),
            ("evt_3".to_string(), 3)
        ]
    );
    let seen = f.seen.lock().unwrap().clone();
    let u = url_of(&seen[0].target);
    assert_eq!(u.path(), "/@portal/rollups/orders_hourly/subscribe");
    assert_eq!(u.query(), Some("fromSequence=7"));
    assert_eq!(seen[1].last_event_id.as_deref(), Some("cur-evt_2"));
    assert!(
        url_of(&seen[1].target).query().is_none(),
        "fromSequence is for the first connection only"
    );
}

#[tokio::test]
async fn rollup_subscribe_resync_calls_the_handler_and_continues() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"reread-source\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", cell("evt_1", 1)),
        Script::Reply(200, "text/event-stream", resync),
        Script::Hang(cell("evt_9", 9)),
    ])
    .await;
    let c = client(&f);
    let calls: Arc<Mutex<u32>> = Arc::default();
    let n = calls.clone();
    let opts = RollupSubscribeOptions {
        from_sequence: None,
        feed: SubscribeOptions::new()
            .reconnect_delay(Duration::from_millis(10))
            .on_resync(move |r| {
                let n = n.clone();
                async move {
                    assert_eq!(r.action, "reread-source");
                    *n.lock().unwrap() += 1;
                }
            }),
    };
    let mut sub = c.rollups().subscribe("r", opts).await.unwrap();
    for want in ["evt_1", "evt_9"] {
        let ev = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ev.event_id, want);
    }
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn rollup_subscribe_errors_on_open_are_typed() {
    for (status, code) in [
        (404, "K9O_NOT_FOUND"),
        (409, "K9O_STATE_CONFLICT"),
        (429, "K9O_SUBSCRIPTION_LIMIT_EXCEEDED"),
    ] {
        let f = fake(vec![Script::Reply(
            status,
            "application/json",
            format!("{{\"code\":\"{code}\",\"message\":\"x\"}}"),
        )])
        .await;
        let c = client(&f);
        let e = c
            .rollups()
            .subscribe("r", RollupSubscribeOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!((e.code.as_str(), e.status), (code, Some(status)));
    }
}

// ---- regression: frames captured live from the gateway ---------------------------------------

const LIVE_FRAME: &str = "retry: 1000\n:ok\n\nid: evt_TqfKnZdBRtggfnwJvuvroU\nevent: data.write.forge.scheduled_trigger.created\ndata: {\"topic\":\"audit.project.data.write.forge.scheduled_trigger.created\",\"eventType\":\"data.write.forge.scheduled_trigger.created\",\"payload\":{\"auditEventId\":\"evt_UNs3pW7TSadU\",\"type\":\"data.write.forge.scheduled_trigger.created\",\"actorType\":\"user\",\"occurredAt\":\"2026-10-27T19:31:40.636Z\",\"outcome\":null},\"occurredAt\":\"2026-10-27T19:31:40.636Z\",\"eventId\":\"evt_TqfKnZdBRtggfnwJvuvroU\"}\n\n";

#[tokio::test]
async fn the_live_frame_yields_the_flattened_event_and_resumes_with_the_sse_id() {
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", LIVE_FRAME.to_string()),
        Script::Hang(LIVE_FRAME.to_string()),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let ev = next_ev(&mut t).await;
    assert_eq!(ev.audit_event_id, "evt_UNs3pW7TSadU");
    assert_eq!(ev.event_type, "data.write.forge.scheduled_trigger.created");
    assert_eq!(ev.actor_type, "user");
    assert_eq!(ev.occurred_at, "2026-10-27T19:31:40.636Z");
    assert_eq!(ev.outcome, None);
    assert_eq!(ev.event_id.as_deref(), Some("evt_TqfKnZdBRtggfnwJvuvroU"));
    // The reconnect repeats the same frame: dropped by eventId, so nothing more arrives.
    assert!(tokio::time::timeout(Duration::from_millis(300), t.next())
        .await
        .is_err());
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].last_event_id.as_deref(),
        Some("evt_TqfKnZdBRtggfnwJvuvroU")
    );
}

#[tokio::test]
async fn replay_decodes_the_live_page_shape() {
    let body = json!({"data": {"topic": "audit.project.*", "events": [
        {"sequence": "1", "topic": "audit.project.data.write.pulse_stream.created",
         "eventType": "data.write.pulse_stream.created",
         "payload": {"auditEventId": "evt_UGgF77wit3Xx", "type": "data.write.pulse_stream.created",
                     "actorType": "user", "occurredAt": "2026-10-27T19:00:00.000Z", "outcome": null},
         "occurredAt": "2026-10-27T19:00:00.000Z"}
    ], "nextCursor": null, "hasMore": true}, "meta": {}})
    .to_string();
    let f = fake(vec![Script::Reply(200, "application/json", body)]).await;
    let c = client(&f);
    let r = c
        .audit()
        .replay(AuditReplayOptions {
            topic: "audit.project.*".into(),
            from_sequence: Some(0),
            limit: Some(3),
            ..Default::default()
        })
        .await
        .unwrap()
        .data;
    assert_eq!(r.events[0].audit_event_id, "evt_UGgF77wit3Xx");
    assert_eq!(r.events[0].event_type, "data.write.pulse_stream.created");
    assert_eq!(r.events[0].sequence.as_deref(), Some("1"));
    assert_eq!(r.events[0].outcome, None);
    assert!(r.has_more && r.next_cursor.is_none());
    let s = f.seen.lock().unwrap()[0].clone();
    let sent: Value = serde_json::from_str(&s.body).unwrap();
    assert_eq!(
        sent,
        json!({"topic": "audit.project.*", "fromSequence": 0, "limit": 3})
    );
}

fn page(events: &[(&str, &str)], more: bool) -> String {
    let evs: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(i, (id, kind))| {
            json!({"sequence": (i + 1).to_string(), "topic": format!("audit.project.{kind}"), "eventType": kind,
                   "payload": {"auditEventId": id, "type": kind, "actorType": "user", "occurredAt": "o", "outcome": null},
                   "occurredAt": "o"})
        })
        .collect();
    json!({"data": {"topic": "audit.project.*", "events": evs, "nextCursor": null, "hasMore": more}, "meta": {}})
        .to_string()
}

#[tokio::test]
async fn resync_pages_the_replay_while_has_more_and_dedupes_the_fresh_connection() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", resync),
        // The live frame for D repeats an audit event the replay delivers: dropped by auditEventId.
        Script::Hang(format!(
            "{}{}",
            frame("evt_D", "t.d"),
            frame("evt_E", "t.e")
        )),
        Script::Reply(
            200,
            "application/json",
            page(&[("evt_B", "t.b"), ("evt_C", "t.c")], true),
        ),
        Script::Reply(200, "application/json", page(&[("evt_D", "t.d")], false)),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(next_ev(&mut t).await.audit_event_id);
    }
    assert_eq!(ids, ["evt_A", "evt_B", "evt_C", "evt_D", "evt_E"]);
    let seen = f.seen.lock().unwrap().clone();
    let b3: Value = serde_json::from_str(&seen[3].body).unwrap();
    let b4: Value = serde_json::from_str(&seen[4].body).unwrap();
    assert_eq!(b3["afterAuditEventId"], "evt_A");
    assert_eq!(b4["afterAuditEventId"], "evt_C");
    assert_eq!(seen[2].method, "GET");
}

#[tokio::test]
async fn caught_up_events_have_a_sequence_and_no_frame_id() {
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n"
            .to_string();
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", resync),
        Script::Hang(String::new()),
        Script::Reply(200, "application/json", page(&[("evt_B", "t.b")], false)),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let _ = next_ev(&mut t).await;
    let b = next_ev(&mut t).await;
    assert_eq!(b.audit_event_id, "evt_B");
    assert_eq!(b.sequence.as_deref(), Some("1"));
    assert_eq!(b.event_id, None);
}

#[tokio::test]
async fn rollup_live_frames_yield_cells_and_suppressed_is_typed() {
    let live = "retry: 1000\n:ok\n\n:hb\n\n";
    let f = fake(vec![Script::Hang(format!(
        "{live}{}{}",
        cell("evt_1", 4),
        suppressed("evt_2")
    ))])
    .await;
    let c = client(&f);
    let mut sub = c
        .rollups()
        .subscribe("lookout-kanon", RollupSubscribeOptions::default())
        .await
        .unwrap();
    let a = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!((a.cell_key.as_str(), a.event_id.as_str()), ("k4", "evt_1"));
    assert_eq!(a.bucket, "2026-10-27T10:00:00Z");
    assert_eq!(a.dims["region"], "de");
    assert!(!a.is_suppressed() && a.status.is_none());
    let b = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(b.is_suppressed());
    assert_eq!(b.reason.as_deref(), Some("cohort_below_k"));
    assert!(b.measures.is_none());
}

const RESYNC: &str =
    "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"replay-ledger\"}\n\n";

#[tokio::test]
async fn an_event_after_the_replays_last_one_is_in_the_fresh_backlog_and_yielded_once() {
    // E was published after the replay's last event (C) but before the fresh stream's first frame:
    // the fresh stream's backlog repeats B and C and carries E.
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", RESYNC.to_string()),
        Script::Hang(format!(
            "{}{}{}",
            frame("evt_B", "t.b"),
            frame("evt_C", "t.c"),
            frame("evt_E", "t.e")
        )),
        Script::Reply(
            200,
            "application/json",
            page(&[("evt_B", "t.b"), ("evt_C", "t.c")], false),
        ),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(next_ev(&mut t).await.audit_event_id);
    }
    assert_eq!(ids, ["evt_A", "evt_B", "evt_C", "evt_E"]);
    // Nothing more: E is not repeated.
    assert!(tokio::time::timeout(Duration::from_millis(300), t.next())
        .await
        .is_err());
}

#[tokio::test]
async fn frames_buffered_during_the_replay_follow_the_replay_events_deduped() {
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", RESYNC.to_string()),
        Script::Hang(format!(
            "{}{}",
            frame("evt_C", "t.c"),
            frame("evt_D", "t.d")
        )),
        Script::Reply(
            200,
            "application/json",
            page(&[("evt_B", "t.b"), ("evt_C", "t.c")], false),
        ),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..4 {
        let e = next_ev(&mut t).await;
        got.push((e.audit_event_id, e.sequence.is_some()));
    }
    assert_eq!(
        got,
        [
            ("evt_A".to_string(), false),
            ("evt_B".to_string(), true),
            ("evt_C".to_string(), true),
            ("evt_D".to_string(), false)
        ]
    );
}

#[tokio::test]
async fn a_resync_with_nothing_delivered_skips_the_replay() {
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", RESYNC.to_string()),
        Script::Hang(format!(
            "{}{}",
            frame("evt_A", "t.a"),
            frame("evt_B", "t.b")
        )),
    ])
    .await;
    let c = client(&f);
    let mut t = c.audit().tail(fast()).await.unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_B");
    let seen = f.seen.lock().unwrap().clone();
    assert!(seen.iter().all(|s| s.method == "GET"));
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.last_event_id.is_none()));
}

const HEAD_RESYNC: &str =
    "event: resync\ndata: {\"reason\":\"cursor-evicted\",\"head\":75,\"action\":\"replay-ledger\"}\n\n";

#[tokio::test]
async fn a_head_resync_on_a_fresh_connection_is_informational() {
    // The captured stream: resync (head) first, then the lookback and live events, one connection.
    let f = fake(vec![Script::Hang(format!(
        "{}{}{}",
        HEAD_RESYNC,
        frame("evt_A", "t.a"),
        frame("evt_B", "t.b")
    ))])
    .await;
    let c = client(&f);
    let calls: Arc<Mutex<u32>> = Arc::default();
    let n = calls.clone();
    let opts = fast().on_resync(move |_| {
        let n = n.clone();
        async move {
            *n.lock().unwrap() += 1;
        }
    });
    let mut t = c.audit().tail(opts).await.unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_B");
    assert_eq!(*calls.lock().unwrap(), 0);
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
}

#[tokio::test]
async fn a_double_resync_backs_off_and_keeps_trying() {
    let two = format!("{HEAD_RESYNC}{RESYNC}");
    let f = fake(vec![
        Script::Reply(200, "text/event-stream", frame("evt_A", "t.a")),
        Script::Reply(200, "text/event-stream", RESYNC.to_string()),
        // Each fresh connection opens with the informational head resync, then a real one.
        Script::Reply(200, "text/event-stream", two.clone()),
        Script::Reply(200, "application/json", page(&[], false)),
        Script::Reply(200, "text/event-stream", two),
        Script::Reply(200, "application/json", page(&[], false)),
        Script::Hang(frame("evt_B", "t.b")),
        Script::Reply(200, "application/json", page(&[], false)),
    ])
    .await;
    let c = client(&f);
    let opts = AuditTailOptions::new().reconnect_delay(Duration::from_millis(100));
    let mut t = c.audit().tail(opts).await.unwrap();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_A");
    let started = std::time::Instant::now();
    assert_eq!(next_ev(&mut t).await.audit_event_id, "evt_B");
    // Three resyncs in a row with no event between: the pauses grow (100, 200, 400 ms).
    assert!(started.elapsed() >= Duration::from_millis(550));
}

#[tokio::test]
async fn audit_replay_defaults_to_1000_and_refuses_out_of_range_limits() {
    let f = fake(vec![Script::Reply(
        200,
        "application/json",
        page(&[("evt_B", "t.b")], false),
    )])
    .await;
    let c = client(&f);
    let opts = |limit| AuditReplayOptions {
        topic: "audit.project.*".into(),
        after_audit_event_id: Some("evt_A".into()),
        limit,
        ..Default::default()
    };
    c.audit().replay(opts(None)).await.unwrap();
    let body: Value = serde_json::from_str(&f.seen.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["limit"], 1000);
    for bad in [10001u32, 0] {
        let e = c.audit().replay(opts(Some(bad))).await.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    }
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}
