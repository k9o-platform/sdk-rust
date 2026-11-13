//! Realtime change feed: reconnect with Last-Event-ID, duplicate drop, resync,
//! heartbeats, error handling and idle timeout. Uses a scripted local TCP
//! server so connections can stay open, close, or go quiet on demand.

use k9o::{Client, ClientOpts, SubscribeOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

enum Script {
    /// Answer with a status and a body, then close.
    Reply(u16, String),
    /// Send headers and `body`, then stay open and silent.
    Hang(String),
}

struct Fake {
    uri: String,
    /// Last-Event-ID header of each request (None when absent).
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

async fn fake(scripts: Vec<Script>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
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
            let last = head
                .lines()
                .find(|l| l.to_lowercase().starts_with("last-event-id:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            log.lock().unwrap().push(last);
            // Past the script: answer 200 and stay silent.
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
                        let _ = sock.read(&mut b).await; // until the client leaves
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
        ..Default::default()
    })
    .unwrap()
}

fn frame(id: &str, rec: &str) -> String {
    format!(
        "id: {id}\nevent: row_inserted\ndata: {{\"topic\":\"collections.kv.row_inserted\",\"eventType\":\"row_inserted\",\"payload\":{{\"collection\":\"kv\",\"id\":\"{rec}\"}},\"occurredAt\":\"2026-10-27T10:20:38.969Z\",\"eventId\":\"{id}\"}}\n\n"
    )
}

fn fast() -> SubscribeOptions {
    SubscribeOptions::new().reconnect_delay(Duration::from_millis(10))
}

async fn next_ev(sub: &mut k9o::Subscription) -> k9o::RealtimeEvent {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .expect("closed")
        .expect("error")
}

#[tokio::test]
async fn frame_has_event_id_and_heartbeats_are_ignored() {
    let body = format!("retry: 1000\n:ok\n\n{}:hb\n\n", frame("evt_A", "rec_1"));
    let f = fake(vec![Script::Hang(body)]).await;
    let c = client(&f);
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .unwrap();
    let ev = next_ev(&mut sub).await;
    assert_eq!(ev.event_id, "evt_A");
    assert_eq!(ev.event_type, "row_inserted");
    assert_eq!(ev.payload["id"], "rec_1");
    assert_eq!(f.seen.lock().unwrap()[0], None);
}

#[tokio::test]
async fn reconnects_with_last_event_id_and_drops_duplicates() {
    let first = format!("{}{}:hb\n\n", frame("evt_A", "r1"), frame("evt_B", "r2"));
    // The resend repeats evt_A and evt_B (lookback), then one new event.
    let second = format!(
        "{}{}{}",
        frame("evt_A", "r1"),
        frame("evt_B", "r2"),
        frame("evt_C", "r3")
    );
    let f = fake(vec![Script::Reply(200, first), Script::Hang(second)]).await;
    let c = client(&f);
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .unwrap();
    let ids = [
        next_ev(&mut sub).await.event_id,
        next_ev(&mut sub).await.event_id,
        next_ev(&mut sub).await.event_id,
    ];
    assert_eq!(ids, ["evt_A", "evt_B", "evt_C"]);
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![None, Some("evt_B".to_string())]);
}

#[tokio::test]
async fn resync_calls_handler_is_not_yielded_and_next_connection_has_no_last_event_id() {
    let first = frame("evt_A", "r1");
    let resync =
        "event: resync\ndata: {\"reason\":\"cursor-unknown\",\"action\":\"reread-source\"}\n\n"
            .to_string();
    let second = format!("{}", resync);
    // After the resync the same id may legitimately come back: dedupe was cleared.
    let third = frame("evt_A", "r1");
    let f = fake(vec![
        Script::Reply(200, first),
        Script::Reply(200, second),
        Script::Hang(third),
    ])
    .await;
    let c = client(&f);
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = calls.clone();
    let opts = fast().on_resync(move |r| {
        let log = log.clone();
        async move {
            tokio::task::yield_now().await;
            log.lock().unwrap().push((r.reason, r.action));
        }
    });
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", opts)
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    // The next event only comes from the third connection, after the resync.
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![("cursor-unknown".to_string(), "reread-source".to_string())]
    );
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![None, Some("evt_A".to_string()), None]);
}

#[tokio::test]
async fn client_error_is_thrown_once_and_not_retried() {
    let body = "{\"code\":\"K9O_UNAUTHORIZED\",\"message\":\"no\"}".to_string();
    let f = fake(vec![Script::Reply(401, body)]).await;
    let c = client(&f);
    let err = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .err()
        .expect("401 must be an error");
    assert_eq!(err.status, Some(401));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn client_error_on_reconnect_ends_the_subscription_once() {
    let body = "{\"code\":\"K9O_FORBIDDEN\",\"message\":\"no\"}".to_string();
    let f = fake(vec![
        Script::Reply(200, frame("evt_A", "r1")),
        Script::Reply(403, body),
    ])
    .await;
    let c = client(&f);
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .unwrap();
    next_ev(&mut sub).await;
    let e = sub.next().await.unwrap().unwrap_err();
    assert_eq!(e.status, Some(403));
    assert!(sub.next().await.is_none());
    assert_eq!(f.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn server_error_is_retried() {
    let f = fake(vec![
        Script::Reply(
            503,
            "{\"code\":\"K9O_UPSTREAM_UNAVAILABLE\",\"message\":\"x\"}".into(),
        ),
        Script::Reply(503, "oops".into()),
        Script::Hang(frame("evt_A", "r1")),
    ])
    .await;
    let c = client(&f);
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert_eq!(f.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn close_stops_reconnecting() {
    let f = fake(vec![
        Script::Reply(200, frame("evt_A", "r1")),
        Script::Hang(String::new()),
    ])
    .await;
    let c = client(&f);
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", fast())
        .await
        .unwrap();
    next_ev(&mut sub).await;
    sub.close();
    assert!(sub.next().await.is_none());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn idle_connection_is_abandoned_and_reconnected() {
    let f = fake(vec![
        Script::Hang(frame("evt_A", "r1")), // then silent
        Script::Hang(frame("evt_B", "r2")),
    ])
    .await;
    let c = client(&f);
    let opts = fast().idle_timeout(Duration::from_millis(150));
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", opts)
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_B");
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![None, Some("evt_A".to_string())]);
}

const HEAD_RESYNC: &str =
    "event: resync\ndata: {\"reason\":\"cursor-evicted\",\"head\":75,\"action\":\"reread-source\"}\n\n";

type ResyncLog = Arc<Mutex<Vec<String>>>;

fn logging(log: &ResyncLog) -> SubscribeOptions {
    let log = log.clone();
    fast().on_resync(move |r| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(r.reason);
        }
    })
}

#[tokio::test]
async fn opening_resync_on_a_fresh_connection_is_informational_and_events_follow() {
    // retry/:ok, then the head resync, then an event, all on one connection.
    let body = format!(
        "retry: 1000\n:ok\n\n{HEAD_RESYNC}{}",
        frame("evt_A", "rec_1")
    );
    let f = fake(vec![Script::Hang(body)]).await;
    let c = client(&f);
    let log = ResyncLog::default();
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", logging(&log))
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert!(
        log.lock().unwrap().is_empty(),
        "on_resync must not be called"
    );
    assert_eq!(f.seen.lock().unwrap().len(), 1, "no reconnect");
}

#[tokio::test]
async fn resync_after_an_event_is_a_real_resync() {
    let first = format!("{}{HEAD_RESYNC}", frame("evt_A", "rec_1"));
    let f = fake(vec![
        Script::Reply(200, first),
        Script::Hang(frame("evt_B", "rec_2")),
    ])
    .await;
    let c = client(&f);
    let log = ResyncLog::default();
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", logging(&log))
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_B");
    assert_eq!(
        log.lock().unwrap().clone(),
        vec!["cursor-evicted".to_string()]
    );
    assert_eq!(f.seen.lock().unwrap().len(), 2, "reconnected");
}

#[tokio::test]
async fn resync_on_a_connection_that_sent_last_event_id_is_a_real_resync() {
    // Connection 1 delivers an event and closes; connection 2 sends Last-Event-ID and opens with a resync.
    let f = fake(vec![
        Script::Reply(200, frame("evt_A", "rec_1")),
        Script::Reply(200, HEAD_RESYNC.to_string()),
        Script::Hang(frame("evt_B", "rec_2")),
    ])
    .await;
    let c = client(&f);
    let log = ResyncLog::default();
    let mut sub = c
        .realtime()
        .subscribe_with("collections.kv.*", logging(&log))
        .await
        .unwrap();
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_A");
    assert_eq!(next_ev(&mut sub).await.event_id, "evt_B");
    assert_eq!(
        log.lock().unwrap().clone(),
        vec!["cursor-evicted".to_string()]
    );
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen, vec![None, Some("evt_A".to_string()), None]);
}
