//! Realtime Subscriptions namespace — `client.realtime()`.
//!
//! `subscribe` returns a [`Subscription`] async-iterated via `next().await`.
//! Transport: SSE over `GET /@<slug>/realtime/subscribe`. The subscription
//! reconnects by itself with `Last-Event-ID`, drops events it already
//! delivered (by `event_id`), ignores heartbeats, abandons a connection that
//! goes quiet for 45 seconds, and calls `on_resync` when the server loses the
//! position (the caller re-reads current state there).

use crate::envelope::Response;
use crate::http::HttpClientConfig;
use crate::kerror::KError;
use crate::stream::{open_sse_response, require_project};
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

/// Event ids kept for duplicate detection.
const SEEN_CAPACITY: usize = 1024;
/// Longest pause between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Reconnect pause when the server sends no `retry:` hint.
const DEFAULT_RETRY: Duration = Duration::from_millis(1000);
/// A connection that delivers no bytes for this long is abandoned.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// A single realtime event delivered on a subscription.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeEvent {
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub occurred_at: String,
    /// Unique id of the event (`evt_...`). Delivery is at least once: if you
    /// also store events, dedupe by this id.
    #[serde(default)]
    pub event_id: String,
}

/// Typed builders for the 6 closed topic families.
pub mod topic {
    pub fn collection(collection: &str) -> String {
        format!("collections.{}.*", collection)
    }
    pub fn workflow_runs() -> String {
        "studio.workflows.runs.*".to_string()
    }
    pub fn query_executed(query_id: &str) -> String {
        format!("studio.queries.executed.{}", query_id)
    }
    pub fn dashboard_refresh(dashboard_id: &str) -> String {
        format!("studio.dashboards.refresh.{}", dashboard_id)
    }
    pub fn function_invocations(function_id: &str) -> String {
        format!("forge.functions.invocations.{}", function_id)
    }
    pub fn audit_org(event_family: &str) -> String {
        format!("audit.org.{}", event_family)
    }
}

/// What the server said when it lost the subscription's position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resync {
    /// Why the position was lost, for example `cursor-unknown`.
    pub reason: String,
    /// What to do about it, for example `reread-source`.
    pub action: String,
}

/// Boxed future returned by an `on_resync` handler.
pub type ResyncFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

type ResyncHandler = Arc<dyn Fn(Resync) -> ResyncFuture + Send + Sync>;

/// Options for [`RealtimeNamespace::subscribe_with`].
#[derive(Clone, Default)]
pub struct SubscribeOptions {
    /// Narrows the subscription (it composes with RLS: the filter narrows,
    /// RLS gates). Sent as JSON.
    pub filter: Option<Value>,
    /// Called when the server cannot resume from the last event. Re-read
    /// current state through the data API inside it; the subscription then
    /// continues on a fresh connection.
    pub on_resync: Option<ResyncHandler>,
    /// Pause before a reconnect. Default: the server's `retry:` hint, else one
    /// second; repeated failures double it up to 30 seconds.
    pub reconnect_delay: Option<Duration>,
    /// Reconnect when no bytes (events, comments or heartbeats) arrive for
    /// this long. Default 45 seconds.
    pub idle_timeout: Option<Duration>,
}

impl SubscribeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn filter(mut self, filter: Value) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Set the `on_resync` handler. It may be async.
    pub fn on_resync<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Resync) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_resync = Some(Arc::new(move |r| Box::pin(handler(r))));
        self
    }

    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = Some(delay);
        self
    }

    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }
}

impl std::fmt::Debug for SubscribeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubscribeOptions")
            .field("filter", &self.filter)
            .field("on_resync", &self.on_resync.as_ref().map(|_| "<fn>"))
            .field("reconnect_delay", &self.reconnect_delay)
            .field("idle_timeout", &self.idle_timeout)
            .finish()
    }
}

/// A live subscription over SSE. Iterate with `next().await`. It reconnects
/// on its own (see the module docs) until `close()` or until an error that a
/// retry cannot fix.
pub struct Subscription {
    config: HttpClientConfig,
    path: String,
    query: Vec<(String, String)>,
    /// Extra query parameters for the first connection only (for example `fromSequence`).
    initial_query: Vec<(String, String)>,
    initial_spent: bool,
    opts: SubscribeOptions,
    response: Option<reqwest::Response>,
    buffer: Vec<u8>,
    last_id: Option<String>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    retry_hint: Option<Duration>,
    failures: u32,
    wait_before_connect: bool,
    got_block: bool,
    done: bool,
    kind: FeedKind,
    /// Audit tail only: the topic, and the last audit event id delivered (live or replayed).
    audit: Option<AuditState>,
    /// Audit tail only: events fetched by a replay, delivered before any live frame.
    pending: VecDeque<RealtimeEvent>,
    /// Audit tail only: a resync happened and the replay catch-up has not run yet. It runs right
    /// after the next fresh connection is open, while that connection's frames are buffered.
    catchup_needed: bool,
    /// Audit tail only: resyncs in a row with no event delivered in between.
    resync_streak: u32,
    /// This connection sent neither a `Last-Event-ID` nor a start position and has delivered nothing yet, so a `resync` now is
    /// the gateway's informational head marker, not a lost position. Applies to every feed kind.
    fresh_head: bool,
}

/// What the frames of a feed look like.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedKind {
    /// `{ topic, eventType, payload, occurredAt, eventId }` frames.
    Realtime,
    /// The project audit tail: the same frame, with the five audit fields in `payload`.
    Audit,
    /// Rollup cells: one `cell` event per closed cell, the cell is the frame's `payload`.
    Rollup,
}

pub(crate) struct AuditState {
    pub topic: String,
    pub last_audit_id: Option<String>,
}

fn error_status(e: &KError) -> Option<u64> {
    e.status
        .map(u64::from)
        .or_else(|| e.context.get("status").and_then(Value::as_u64))
}

/// Retry transport errors and 5xx; anything else (4xx) is final.
fn is_retryable(e: &KError) -> bool {
    match error_status(e) {
        Some(s) => s >= 500,
        None => e.code == "K9O_UPSTREAM_UNAVAILABLE",
    }
}

impl Subscription {
    fn idle(&self) -> Duration {
        self.opts.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT)
    }

    fn delay(&self) -> Duration {
        let base = self
            .opts
            .reconnect_delay
            .or(self.retry_hint)
            .unwrap_or(DEFAULT_RETRY);
        let shift = self.failures.saturating_sub(1).min(16);
        base.saturating_mul(1u32 << shift).min(MAX_BACKOFF)
    }

    fn remember(&mut self, id: &str) {
        if self.seen.insert(id.to_string()) {
            self.seen_order.push_back(id.to_string());
            while self.seen_order.len() > SEEN_CAPACITY {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }

    /// One connection attempt. Ok(()) stores the response.
    async fn connect(&mut self) -> Result<(), KError> {
        let last = self.last_id.clone();
        let headers: Vec<(&str, &str)> = last
            .as_deref()
            .map(|id| vec![("last-event-id", id)])
            .unwrap_or_default();
        let mut query = self.query.clone();
        let mut has_start_position = false;
        if !self.initial_spent && last.is_none() {
            has_start_position = !self.initial_query.is_empty();
            query.extend(self.initial_query.iter().cloned());
        }
        let attempt = open_sse_response::<()>(
            &self.config,
            Method::GET,
            &self.path,
            None,
            Some(&query),
            &headers,
        );
        let res = match tokio::time::timeout(self.idle(), attempt).await {
            Ok(r) => r,
            Err(_) => Err(KError::new(
                "K9O_UPSTREAM_UNAVAILABLE",
                "timed out connecting to the realtime feed",
            )),
        };
        self.response = Some(res?);
        // A start position (`fromSequence`) is a cursor like Last-Event-ID: an opening resync on
        // such a connection is a real lost position, not the head marker.
        self.fresh_head = last.is_none() && !has_start_position;
        self.buffer.clear();
        self.got_block = false;
        Ok(())
    }

    /// Yield the next event, or `None` once the subscription is closed.
    /// `Some(Err(_))` ends it (for example a 4xx on connect); transport
    /// errors, 5xx answers, the server's 300 second bound and idle
    /// connections are retried without surfacing. A `resync` frame calls
    /// `on_resync` and continues on a fresh connection; it is not yielded. The one exception is the
    /// `resync` that opens a connection sent without `Last-Event-ID` (the gateway's head marker):
    /// it is informational, the stream continues and `on_resync` is not called.
    pub async fn next(&mut self) -> Option<Result<RealtimeEvent, KError>> {
        loop {
            if self.done {
                return None;
            }
            if let Some(ev) = self.pending.pop_front() {
                return Some(Ok(ev));
            }
            // Complete blocks already buffered (frames that arrived during a catch-up).
            if let Some(r) = self.process_buffered().await {
                return Some(r);
            }
            if self.response.is_none() {
                if self.wait_before_connect {
                    tokio::time::sleep(self.delay()).await;
                }
                self.wait_before_connect = true;
                match self.connect().await {
                    Ok(()) => {}
                    Err(e) if is_retryable(&e) => {
                        self.failures += 1;
                        continue;
                    }
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
            }
            if self.catchup_needed && self.response.is_some() {
                if let Err(e) = self.catch_up_from_ledger().await {
                    self.done = true;
                    return Some(Err(e));
                }
                continue;
            }
            if self.response.is_none() {
                continue;
            }
            let idle = self.idle();
            let chunk = {
                let resp = self.response.as_mut().expect("connected");
                tokio::time::timeout(idle, resp.chunk()).await
            };
            match chunk {
                Ok(Ok(Some(bytes))) => self.buffer.extend_from_slice(&bytes),
                Ok(Ok(None)) => {
                    // Server closed the stream (the 300 second bound).
                    self.response = None;
                    if !self.got_block {
                        self.failures += 1;
                    }
                }
                Ok(Err(_)) | Err(_) => {
                    // Network error, or no bytes for the idle limit.
                    self.response = None;
                    self.failures += 1;
                }
            }
        }
    }

    /// Handle the complete blocks in the buffer. `Some` is an event to yield or the error that ends
    /// the feed; `None` once no complete block is left (or a resync dropped the connection).
    async fn process_buffered(&mut self) -> Option<Result<RealtimeEvent, KError>> {
        while let Some((raw, advance)) = take_block(&self.buffer) {
            self.buffer.drain(..advance);
            self.got_block = true;
            self.failures = 0;
            match self.handle_block(&String::from_utf8_lossy(&raw)).await {
                Handled::Event(ev) => {
                    self.resync_streak = 0;
                    return Some(Ok(ev));
                }
                Handled::Resync => {
                    // A fresh connection answered with a resync again, with no event in between:
                    // lengthen the pause before the next attempt so the tail never spins.
                    self.failures = self.resync_streak;
                    return None;
                }
                Handled::Skip => {}
            }
        }
        None
    }

    async fn handle_block(&mut self, raw: &str) -> Handled {
        let mut event = None;
        let mut sse_id = None;
        let mut data: Vec<&str> = Vec::new();
        for line in raw.split('\n') {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => event = Some(value),
                "id" => sse_id = Some(value),
                "data" => data.push(value),
                "retry" => {
                    if let Ok(ms) = value.trim().parse::<u64>() {
                        self.retry_hint = Some(Duration::from_millis(ms));
                    }
                }
                _ => {}
            }
        }
        if event == Some("resync") && self.fresh_head {
            // A fresh connection opens with a resync that only marks the live head: keep reading
            // the same stream; no handler call, no reconnect, no back-off. Every feed (realtime,
            // metadata, rollups, audit tail) treats it this way; a resync after data, or on a
            // connection that sent a Last-Event-ID, is a real lost position.
            self.fresh_head = false;
            return Handled::Skip;
        }
        if event == Some("resync") {
            self.fresh_head = false;
            let v: Value = serde_json::from_str(&data.join("\n")).unwrap_or(Value::Null);
            let text = |k: &str| {
                v.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let resync = Resync {
                reason: text("reason"),
                action: text("action"),
            };
            // The position is lost: forget it and start over.
            self.initial_spent = true;
            self.last_id = None;
            self.response = None;
            self.buffer.clear();
            if self.kind == FeedKind::Audit {
                // Audit streams cannot be resumed by sequence. Reconnect fresh (the gateway first
                // opens at the live head), then replay from the last delivered event while
                // that connection's frames are buffered. The duplicate set is kept: the lookback,
                // the replay and the live frames may repeat events.
                self.catchup_needed = true;
                self.resync_streak += 1;
            } else {
                self.seen.clear();
                self.seen_order.clear();
            }
            if let Some(h) = self.opts.on_resync.clone() {
                h(resync).await;
            }
            return Handled::Resync;
        }
        if data.is_empty() {
            return Handled::Skip;
        }
        let Ok(raw_json) = serde_json::from_str::<Value>(&data.join("\n")) else {
            return Handled::Skip;
        };
        let frame_str = |k: &str| {
            raw_json
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut audit_id: Option<String> = None;
        let mut ev = match self.kind {
            FeedKind::Realtime => {
                if raw_json.get("eventType").and_then(Value::as_str).is_none() {
                    return Handled::Skip;
                }
                let Ok(ev) = serde_json::from_value::<RealtimeEvent>(raw_json) else {
                    return Handled::Skip;
                };
                ev
            }
            FeedKind::Audit => {
                let Some(p) = raw_json.get("payload").filter(|p| p.is_object()) else {
                    return Handled::Skip;
                };
                let Some(id) = p.get("auditEventId").and_then(Value::as_str) else {
                    return Handled::Skip;
                };
                let kind = p
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(frame_str("eventType")).filter(|s| !s.is_empty()))
                    .or_else(|| event.map(str::to_string))
                    .unwrap_or_default();
                let occurred = p
                    .get("occurredAt")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| frame_str("occurredAt"));
                let event_id = frame_str("eventId");
                audit_id = Some(id.to_string());
                let mut flat = serde_json::json!({
                    "auditEventId": id,
                    "type": kind,
                    "actorType": p.get("actorType").cloned().unwrap_or(Value::Null),
                    "occurredAt": occurred,
                    "outcome": p.get("outcome").cloned().unwrap_or(Value::Null),
                });
                if !event_id.is_empty() {
                    flat["eventId"] = Value::String(event_id.clone());
                }
                if flat["actorType"].is_null() {
                    flat["actorType"] = Value::String(String::new());
                }
                RealtimeEvent {
                    topic: frame_str("topic"),
                    event_type: kind,
                    payload: flat,
                    occurred_at: occurred,
                    event_id,
                }
            }
            FeedKind::Rollup => {
                if let Some(name) = event {
                    if name != "cell" {
                        return Handled::Skip;
                    }
                }
                let Some(cell) = raw_json.get("payload").filter(|p| p.is_object()) else {
                    return Handled::Skip;
                };
                RealtimeEvent {
                    topic: frame_str("topic"),
                    event_type: "cell".into(),
                    payload: cell.clone(),
                    occurred_at: frame_str("occurredAt"),
                    event_id: frame_str("eventId"),
                }
            }
        };
        self.initial_spent = true;
        self.fresh_head = false;
        let sse_id = sse_id.filter(|s| !s.is_empty()).map(str::to_string);
        if ev.event_id.is_empty() {
            ev.event_id = sse_id.clone().unwrap_or_default();
        }
        // Dedupe on the frame's eventId; an audit event also on its auditEventId, which a replay
        // may have delivered already.
        if (!ev.event_id.is_empty() && self.seen.contains(&ev.event_id))
            || audit_id.as_ref().is_some_and(|a| self.seen.contains(a))
        {
            return Handled::Skip;
        }
        if !ev.event_id.is_empty() {
            let id = ev.event_id.clone();
            self.remember(&id);
        }
        if let Some(a) = &audit_id {
            self.remember(a);
        }
        // Resume with the SSE `id`, which is not the eventId on the real wire.
        if let Some(id) = sse_id.or_else(|| Some(ev.event_id.clone()).filter(|s| !s.is_empty())) {
            self.last_id = Some(id);
        }
        if let (Some(a), Some(id)) = (self.audit.as_mut(), audit_id) {
            a.last_audit_id = Some(id);
        }
        Handled::Event(ev)
    }

    /// Audit tail, after a resync, with the fresh connection already open: fetch what happened
    /// since the last delivered event through `POST /@<slug>/realtime/replay` and queue it, oldest
    /// first, ahead of the frames that arrived meanwhile. Those frames are read into the buffer
    /// while the replay runs, so nothing published after the replay's last event is lost. Nothing
    /// delivered yet means nothing to catch up from.
    async fn catch_up_from_ledger(&mut self) -> Result<(), KError> {
        let after = self.audit.as_ref().and_then(|a| a.last_audit_id.clone());
        let (Some(state), Some(after), Some(slug)) =
            (self.audit.as_ref(), after, self.config.project_slug.clone())
        else {
            self.catchup_needed = false;
            return Ok(());
        };
        let topic = state.topic.clone();
        let mut fetch = Box::pin(fetch_ledger(
            self.config.clone(),
            slug,
            topic.clone(),
            after,
        ));
        let mut ended = false;
        let events = {
            let resp = self.response.as_mut().expect("connected");
            let buf = &mut self.buffer;
            std::future::poll_fn(|cx| {
                if let Poll::Ready(r) = fetch.as_mut().poll(cx) {
                    return Poll::Ready(r);
                }
                while !ended {
                    let mut chunk = std::pin::pin!(resp.chunk());
                    match chunk.as_mut().poll(cx) {
                        Poll::Ready(Ok(Some(bytes))) => buf.extend_from_slice(&bytes),
                        Poll::Ready(_) => ended = true,
                        Poll::Pending => break,
                    }
                }
                Poll::Pending
            })
            .await?
        };
        if ended {
            // The connection ended during the replay; its buffered frames are still handled.
            self.response = None;
        }
        self.catchup_needed = false;
        for e in events {
            if let Some(a) = self.audit.as_mut() {
                a.last_audit_id = Some(e.audit_event_id.clone());
            }
            if self.seen.contains(&e.audit_event_id) {
                continue;
            }
            let id = e.audit_event_id.clone();
            self.remember(&id);
            let payload = serde_json::to_value(&e).unwrap_or(Value::Null);
            self.pending.push_back(RealtimeEvent {
                topic: topic.clone(),
                event_type: e.event_type.clone(),
                payload,
                occurred_at: e.occurred_at.clone(),
                event_id: String::new(),
            });
        }
        Ok(())
    }

    /// Stop the subscription: `next` returns `None` from now on and the
    /// connection is dropped.
    pub fn close(&mut self) {
        self.done = true;
        self.response = None;
    }
}

/// Replay pages after `after` (1000 events per call), following `hasMore` with the last event of
/// each page as the next cursor; a page that makes no progress ends it.
async fn fetch_ledger(
    config: HttpClientConfig,
    slug: String,
    topic: String,
    after: String,
) -> Result<Vec<crate::audit::AuditReplayEvent>, KError> {
    let path = project_path(&slug, "realtime/replay");
    let mut out = Vec::new();
    let mut after = Some(after);
    while let Some(id) = after.take() {
        let body = serde_json::json!({ "topic": topic, "afterAuditEventId": id, "limit": 1000 });
        let resp = crate::http::request::<crate::audit::AuditReplay, Value>(
            &config,
            Method::POST,
            &path,
            Some(&body),
            None,
        )
        .await?;
        let more = resp.data.has_more;
        let last = resp.data.events.last().map(|e| e.audit_event_id.clone());
        out.extend(resp.data.events);
        let Some(l) = last else { break };
        if more && l != id {
            after = Some(l);
        }
    }
    Ok(out)
}

enum Handled {
    Event(RealtimeEvent),
    Resync,
    Skip,
}

/// First complete block in `buf` (up to a blank line) and the bytes to drain.
fn take_block(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut i = 0;
    while i < buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((buf[..i].to_vec(), i + 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((buf[..i].to_vec(), i + 4));
        }
        i += 1;
    }
    None
}

/// Input of [`RealtimeNamespace::replay`].
#[derive(Debug, Default, Clone)]
pub struct ReplayOptions {
    /// Start at this sequence (`audit.project.*` topics).
    pub from_sequence: Option<u64>,
    /// Stop at this sequence (`audit.project.*` topics).
    pub until_sequence: Option<u64>,
    /// At most this many events, 1 to 10000. The gateway's default applies when unset.
    pub limit: Option<u32>,
    /// `audit.project.*` topics only: the last audit event id you have (`evt_...`); the replay
    /// starts after it. Not together with `from_sequence`.
    pub after_audit_event_id: Option<String>,
}

/// The answer of [`RealtimeNamespace::replay`]: one page of events in the shape `subscribe` yields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RealtimeReplay {
    pub topic: String,
    pub events: Vec<RealtimeEvent>,
    /// The gateway's cursor for the next page, when it sends one (rendered as text).
    pub next_cursor: Option<String>,
    /// True when the gateway says more events follow.
    pub has_more: bool,
    /// Set to `reread-source` when the topic keeps no replayable history: read the current state
    /// through the data API instead.
    pub resync: Option<String>,
}

impl<'de> Deserialize<'de> for RealtimeReplay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let topic = v
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut events = Vec::new();
        if let Some(list) = v.get("events").and_then(Value::as_array) {
            for e in list {
                let mut ev: RealtimeEvent =
                    serde_json::from_value(e.clone()).map_err(serde::de::Error::custom)?;
                if ev.topic.is_empty() {
                    ev.topic = topic.clone();
                }
                events.push(ev);
            }
        }
        let next_cursor = match v.get("nextCursor") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        Ok(RealtimeReplay {
            topic,
            events,
            next_cursor,
            has_more: v.get("hasMore").and_then(Value::as_bool).unwrap_or(false),
            resync: v.get("resync").and_then(Value::as_str).map(String::from),
        })
    }
}

/// Realtime namespace — `client.realtime()`.
pub struct RealtimeNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> RealtimeNamespace<'a> {
    /// Subscribe to a topic over SSE: `GET /@<slug>/realtime/subscribe?topic=&filter=`.
    /// `filter` narrows the subscription (it composes with RLS: the filter
    /// narrows, RLS gates) and is sent as JSON. See [`Self::subscribe_with`]
    /// for the resync handler and timing options.
    pub async fn subscribe(
        &self,
        topic: &str,
        filter: Option<Value>,
    ) -> Result<Subscription, KError> {
        self.subscribe_with(
            topic,
            SubscribeOptions {
                filter,
                ..Default::default()
            },
        )
        .await
    }

    /// Catch up on a topic: `POST /@<slug>/realtime/replay` with `{ topic, fromSequence?,
    /// untilSequence?, limit?, afterAuditEventId? }`. Returns one page of events, oldest first, in
    /// the shape `subscribe` yields, with `next_cursor` and `has_more`. `topic` is required (1 to
    /// 200 characters). `audit.project.*` topics replay the audit ledger (needs the audit log's
    /// access; pass `after_audit_event_id` or `from_sequence`). The other topics keep no replayable
    /// history: the answer has no events and `resync` is `reread-source`, so read current state
    /// through the data API. Needs realtime access.
    pub async fn replay(
        &self,
        topic: &str,
        options: ReplayOptions,
    ) -> Result<Response<RealtimeReplay>, KError> {
        let slug = require_project(self.config, "realtime")?;
        let invalid =
            |m: &str| KError::new("K9O_INVALID_FIELD_VALUE", format!("realtime.replay: {m}"));
        if topic.is_empty() || topic.len() > 200 {
            return Err(invalid("topic must be 1 to 200 characters"));
        }
        if options.after_audit_event_id.is_some() && options.from_sequence.is_some() {
            return Err(invalid(
                "pass after_audit_event_id or from_sequence, not both",
            ));
        }
        if let Some(id) = &options.after_audit_event_id {
            let ok = id.strip_prefix("evt_").is_some_and(|r| {
                (1..=40).contains(&r.len()) && r.chars().all(|c| c.is_ascii_alphanumeric())
            });
            if !ok {
                return Err(invalid("after_audit_event_id must look like evt_<id>"));
            }
        }
        if let Some(l) = options.limit {
            if !(1..=10000).contains(&l) {
                return Err(invalid("limit must be 1 to 10000"));
            }
        }
        let mut body = serde_json::json!({ "topic": topic });
        if let Some(v) = &options.after_audit_event_id {
            body["afterAuditEventId"] = serde_json::json!(v);
        }
        if let Some(v) = options.from_sequence {
            body["fromSequence"] = serde_json::json!(v);
        }
        if let Some(v) = options.until_sequence {
            body["untilSequence"] = serde_json::json!(v);
        }
        if let Some(v) = options.limit {
            body["limit"] = serde_json::json!(v);
        }
        crate::http::request::<RealtimeReplay, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "realtime/replay"),
            Some(&body),
            None,
        )
        .await
    }

    /// Subscribe with options (`filter`, `on_resync`, `reconnect_delay`,
    /// `idle_timeout`). A client error on the first connect (4xx) is returned
    /// here; a transport error or 5xx is retried by `next`.
    pub async fn subscribe_with(
        &self,
        topic: &str,
        opts: SubscribeOptions,
    ) -> Result<Subscription, KError> {
        let slug = require_project(self.config, "realtime")?;
        let mut query: Vec<(String, String)> = vec![("topic".into(), topic.into())];
        if let Some(f) = &opts.filter {
            query.push(("filter".into(), f.to_string()));
        }
        open_feed(
            self.config,
            project_path(slug, "realtime/subscribe"),
            query,
            Vec::new(),
            opts,
        )
        .await
    }
}

/// Open a change feed at `path` and make the first connection. `initial_query` is sent on the
/// first connection only. A client error on that connect is returned; a transport error or 5xx is
/// retried by `next`.
pub(crate) async fn open_feed(
    config: &HttpClientConfig,
    path: String,
    query: Vec<(String, String)>,
    initial_query: Vec<(String, String)>,
    opts: SubscribeOptions,
) -> Result<Subscription, KError> {
    open_feed_kind(
        config,
        path,
        query,
        initial_query,
        opts,
        FeedKind::Realtime,
        None,
    )
    .await
}

/// [`open_feed`] for a feed whose frames are not realtime events (the audit tail, rollup cells).
pub(crate) async fn open_feed_kind(
    config: &HttpClientConfig,
    path: String,
    query: Vec<(String, String)>,
    initial_query: Vec<(String, String)>,
    opts: SubscribeOptions,
    kind: FeedKind,
    audit: Option<AuditState>,
) -> Result<Subscription, KError> {
    let mut sub = Subscription {
        config: config.clone(),
        path,
        query,
        initial_query,
        initial_spent: false,
        opts,
        response: None,
        buffer: Vec::new(),
        last_id: None,
        seen: HashSet::new(),
        seen_order: VecDeque::new(),
        retry_hint: None,
        failures: 0,
        wait_before_connect: false,
        got_block: false,
        done: false,
        kind,
        audit,
        pending: VecDeque::new(),
        catchup_needed: false,
        resync_streak: 0,
        fresh_head: false,
    };
    match sub.connect().await {
        Ok(()) => sub.wait_before_connect = true,
        Err(e) if is_retryable(&e) => {
            sub.failures = 1;
            sub.wait_before_connect = true;
        }
        Err(e) => return Err(e),
    }
    Ok(sub)
}
