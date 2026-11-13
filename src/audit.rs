//! Project-plane audit log namespace — `client.audit()`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::realtime::{
    open_feed_kind, AuditState, FeedKind, Resync, SubscribeOptions, Subscription,
};
use crate::stream::require_project;
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

/// Audit event record
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub target_kind: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
    pub occurred_at: String,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
}

/// Optional filters for audit queries; names follow the gateway's query parameters.
#[derive(Debug, Default, Clone)]
pub struct AuditQueryOpts<'a> {
    /// Filter by audit-event name (e.g. `data.write.access.insert`).
    pub event_type: Option<&'a str>,
    /// Filter by actor.
    pub actor_id: Option<&'a str>,
    /// ISO 8601 lower bound: only events after this instant.
    pub after: Option<&'a str>,
    /// ISO 8601 upper bound: only events before this instant.
    pub before: Option<&'a str>,
    /// Page size (the per-tier maximum is bounded server-side).
    pub limit: Option<u32>,
    /// Cursor from a prior response.
    pub cursor: Option<&'a str>,
}

impl<'a> AuditQueryOpts<'a> {
    pub(crate) fn to_query(&self) -> Vec<(String, String)> {
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(v) = self.event_type {
            q.push(("eventType".into(), v.into()));
        }
        if let Some(v) = self.actor_id {
            q.push(("actorId".into(), v.into()));
        }
        if let Some(v) = self.after {
            q.push(("after".into(), v.into()));
        }
        if let Some(v) = self.before {
            q.push(("before".into(), v.into()));
        }
        if let Some(v) = self.limit {
            q.push(("limit".into(), v.to_string()));
        }
        if let Some(v) = self.cursor {
            q.push(("cursor".into(), v.into()));
        }
        q
    }
}

/// One event of the live audit tail. The tail flattens the frame's `payload` to these fields; read
/// the full row with [`AuditNamespace::query`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditTailEvent {
    /// The audit event's id (`evt_...`). Delivery is at least once; the tail drops repeats by it.
    pub audit_event_id: String,
    /// The audit event type, for example `data.write.access.insert`.
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub actor_type: String,
    #[serde(default)]
    pub occurred_at: String,
    /// The outcome, or `None` when the gateway sends `null`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// The publisher's id of the frame (also `evt_...`, but not the audit event id). `None` for an
    /// event caught up through the replay endpoint.
    #[serde(default)]
    pub event_id: Option<String>,
    /// The ledger position, as a decimal string; set only on an event caught up through the replay
    /// endpoint.
    #[serde(default)]
    pub sequence: Option<String>,
}

/// One event of a replay: the tail fields flattened from the event's `payload`, plus its `sequence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditReplayEvent {
    pub audit_event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub actor_type: String,
    pub occurred_at: String,
    /// `None` when the gateway sends `null`.
    pub outcome: Option<String>,
    /// The event's position in the project's audit ledger, as a decimal string, when sent.
    pub sequence: Option<String>,
}

/// The answer of [`AuditNamespace::replay`]: one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditReplay {
    pub events: Vec<AuditReplayEvent>,
    /// The gateway's cursor for the next page, when it sends one (rendered as text).
    pub next_cursor: Option<String>,
    /// True when the gateway says more events follow.
    pub has_more: bool,
}

fn text_of(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

impl<'de> Deserialize<'de> for AuditReplay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let list = match &v {
            Value::Array(a) => a.clone(),
            other => other
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        };
        let events = list
            .iter()
            .filter_map(|e| {
                // The fields sit in `payload`; a flat event is read too.
                let src = match e.get("payload") {
                    Some(p) if p.is_object() => p,
                    _ => e,
                };
                let id = src
                    .get("auditEventId")
                    .or_else(|| e.get("auditEventId"))
                    .and_then(Value::as_str)?;
                let field = |k: &str| src.get(k).or_else(|| e.get(k));
                Some(AuditReplayEvent {
                    audit_event_id: id.to_string(),
                    event_type: text_of(field("type"))
                        .or_else(|| text_of(e.get("eventType")))
                        .unwrap_or_default(),
                    actor_type: text_of(field("actorType")).unwrap_or_default(),
                    occurred_at: text_of(field("occurredAt")).unwrap_or_default(),
                    outcome: text_of(field("outcome")),
                    sequence: text_of(e.get("sequence")),
                })
            })
            .collect();
        let has_more = v.get("hasMore").and_then(Value::as_bool).unwrap_or(false);
        let next_cursor = text_of(v.get("nextCursor"));
        Ok(AuditReplay {
            events,
            next_cursor,
            has_more,
        })
    }
}

/// Input of [`AuditNamespace::replay`]. Give `after_audit_event_id` or `from_sequence`, not both.
#[derive(Debug, Default, Clone)]
pub struct AuditReplayOptions {
    /// The topic: `audit.project.*` or `audit.project.<type>`.
    pub topic: String,
    /// The last `audit_event_id` you have (`evt_...`); the replay starts after it. An id that is not
    /// in the project's audit log fails with `K9O_INVALID_CURSOR` (400).
    pub after_audit_event_id: Option<String>,
    /// Start at this ledger sequence.
    pub from_sequence: Option<u64>,
    /// Stop at this ledger sequence.
    pub until_sequence: Option<u64>,
    /// At most this many events, 1 to 10000. Default 1000.
    pub limit: Option<u32>,
}

/// Options of [`AuditNamespace::tail`].
#[derive(Clone, Default)]
pub struct AuditTailOptions {
    types: Vec<String>,
    feed: SubscribeOptions,
}

impl AuditTailOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only these audit event types. None: every type (`audit.project.*`). One type subscribes to
    /// `audit.project.<type>`; several subscribe to `audit.project.*` and the SDK drops the rest.
    pub fn types<I, S>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.types = types.into_iter().map(Into::into).collect();
        self
    }

    /// Called when the server cannot resume the tail. The tail catches up from the audit
    /// ledger by itself (`replay-ledger`); the handler is for your own bookkeeping.
    pub fn on_resync<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Resync) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.feed = self.feed.on_resync(handler);
        self
    }

    /// Pause before a reconnect (default: the server's hint, else one second).
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.feed = self.feed.reconnect_delay(delay);
        self
    }

    /// Reconnect when no bytes arrive for this long (default 45 seconds).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.feed = self.feed.idle_timeout(timeout);
        self
    }
}

/// The live audit tail of the project. Iterate with `next().await`; it reconnects by itself with
/// `Last-Event-ID` (the frame's SSE `id`), drops repeats by the frame's `eventId`, ignores
/// heartbeats and, when the server loses its position, reconnects fresh, catches up through the
/// replay endpoint from the last event it delivered and merges the two without loss. A new
/// connection starts at the live head. Call `close()` to stop.
pub struct AuditTail {
    inner: Subscription,
    only: Option<HashSet<String>>,
}

impl AuditTail {
    /// The next event, or `None` once closed. `Some(Err(_))` ends the tail.
    pub async fn next(&mut self) -> Option<Result<AuditTailEvent, KError>> {
        loop {
            let ev = match self.inner.next().await? {
                Ok(ev) => ev,
                Err(e) => return Some(Err(e)),
            };
            let Ok(tail) = serde_json::from_value::<AuditTailEvent>(ev.payload) else {
                continue;
            };
            if let Some(only) = &self.only {
                if !only.contains(&tail.event_type) {
                    continue;
                }
            }
            return Some(Ok(tail));
        }
    }

    /// Stop the tail: `next` returns `None` from now on.
    pub fn close(&mut self) {
        self.inner.close();
    }
}

fn bad_type(t: &str) -> KError {
    KError::new(
        "K9O_INVALID_FIELD_VALUE",
        format!("audit.tail: invalid event type {:?}", t),
    )
}

fn check_type(t: &str) -> Result<(), KError> {
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(bad_type(t))
    }
}

fn check_event_id(id: &str) -> bool {
    id.strip_prefix("evt_")
        .map(|r| (1..=40).contains(&r.len()) && r.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or(false)
}

/// Audit namespace — `client.audit()` (project-plane).
pub struct AuditNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> AuditNamespace<'a> {
    /// Query the audit log (reverse-chronological; filters AND-compose).
    pub async fn query(
        &self,
        opts: AuditQueryOpts<'_>,
    ) -> Result<Response<Vec<AuditEvent>>, KError> {
        let slug = require_project(self.config, "audit")?;
        request::<Vec<AuditEvent>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "audit-log"),
            None,
            Some(&opts.to_query()),
        )
        .await
    }

    /// Follow the project's audit events as they happen: `GET /@<slug>/realtime/subscribe` on the
    /// topic `audit.project.*` (or `audit.project.<type>` for one type). Each event has
    /// `audit_event_id`, `event_type`, `actor_type`, `occurred_at` and `outcome` (flattened from the
    /// frame's `payload`) plus the frame's `event_id`; read the full row
    /// with [`Self::query`]. `warden.checkpoint` events are not in the live tail; the audit log and
    /// [`Self::replay`] carry them. Needs ORG_OWNER, ORG_ADMIN, PROJECT_ADMIN or PROJECT_AUDITOR
    /// (checked again while the tail is open); extensions cannot subscribe. The first connection is
    /// opened here, so a refusal (403 `K9O_RBAC_DENY`) is returned as `Err`.
    pub async fn tail(&self, options: AuditTailOptions) -> Result<AuditTail, KError> {
        let slug = require_project(self.config, "audit")?;
        for t in &options.types {
            check_type(t)?;
        }
        let topic = match options.types.as_slice() {
            [one] => format!("audit.project.{}", one),
            _ => "audit.project.*".to_string(),
        };
        let only = if options.types.len() > 1 {
            Some(options.types.iter().cloned().collect::<HashSet<_>>())
        } else {
            None
        };
        let inner = open_feed_kind(
            self.config,
            project_path(slug, "realtime/subscribe"),
            vec![("topic".into(), topic.clone())],
            Vec::new(),
            options.feed,
            FeedKind::Audit,
            Some(AuditState {
                topic,
                last_audit_id: None,
            }),
        )
        .await?;
        Ok(AuditTail { inner, only })
    }

    /// `POST /@<slug>/realtime/replay` for an audit topic: a one-shot batch of audit events in
    /// order, `warden.checkpoint` included, returned as a page (`events`, `next_cursor`, `has_more`).
    /// Pass `after_audit_event_id` (the last id you received) or `from_sequence`; for the next page
    /// pass the last event's `audit_event_id`. Needs the audit log's access.
    pub async fn replay(
        &self,
        options: AuditReplayOptions,
    ) -> Result<Response<AuditReplay>, KError> {
        let slug = require_project(self.config, "audit")?;
        let invalid =
            |m: &str| KError::new("K9O_INVALID_FIELD_VALUE", format!("audit.replay: {m}"));
        if options.topic.is_empty() || options.topic.len() > 200 {
            return Err(invalid("topic must be 1 to 200 characters"));
        }
        if options.after_audit_event_id.is_some() && options.from_sequence.is_some() {
            return Err(invalid(
                "pass after_audit_event_id or from_sequence, not both",
            ));
        }
        if let Some(id) = &options.after_audit_event_id {
            if !check_event_id(id) {
                return Err(invalid("after_audit_event_id must look like evt_<id>"));
            }
        }
        if let Some(l) = options.limit {
            if !(1..=10000).contains(&l) {
                return Err(invalid("limit must be 1 to 10000"));
            }
        }
        let mut body = json!({ "topic": options.topic });
        if let Some(v) = &options.after_audit_event_id {
            body["afterAuditEventId"] = json!(v);
        }
        if let Some(v) = options.from_sequence {
            body["fromSequence"] = json!(v);
        }
        if let Some(v) = options.until_sequence {
            body["untilSequence"] = json!(v);
        }
        // No limit: the gateway would answer with every event, so the SDK asks for 1000.
        body["limit"] = json!(options.limit.unwrap_or(1000));
        request::<AuditReplay, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "realtime/replay"),
            Some(&body),
            None,
        )
        .await
    }
}
