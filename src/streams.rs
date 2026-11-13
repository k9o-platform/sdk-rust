//! User data streams namespace — `client.streams()`.
//!
//! Control plane (`create`, `list`, `get`, `delete`) plus the data plane over
//! the gateway's `/@<slug>/data-streams/user/<stream>` routes: `publish`,
//! `replay` (one bounded read) and `subscribe` (SSE live tail).
//!
//! `subscribe` returns a [`StreamSubscription`] iterated with `next().await`.
//! The server closes each connection after a bounded time; the subscription
//! reconnects from the last delivered sequence by itself. Drop it (or call
//! `close`) to stop.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::{open_sse_stream, require_project, SseStream};
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

/// A user data stream as the gateway returns it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub project_id: String,
    pub display_name: String,
    #[serde(default)]
    pub subject_prefix: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub retention_seconds: i64,
    #[serde(default)]
    pub retention_class: String,
    /// `None` when the stream has no storage ceiling.
    #[serde(default)]
    pub max_bytes: Option<i64>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// Result of `streams().delete`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StreamDeleted {
    pub slug: String,
    pub status: String,
}

/// Result of `streams().publish`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub stream_slug: String,
    pub subject: String,
    pub bytes: u64,
    pub accepted: bool,
}

/// One event of a stream. `sequence` is the stream sequence (a decimal
/// string); `data` is the published JSON payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamEvent {
    pub sequence: String,
    pub subject: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub published_at: String,
}

/// A page of events from `streams().replay`. Pass `next_cursor` as the next
/// `from_sequence` while `has_more` is true.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    pub stream_slug: String,
    #[serde(default)]
    pub events: Vec<StreamEvent>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

/// A live tail of a stream. Iterate with `next().await`; it reconnects from the
/// last sequence when the server closes the connection.
pub struct StreamSubscription<'a> {
    config: &'a HttpClientConfig,
    path: String,
    stream: Option<SseStream<StreamEvent>>,
    last: Option<String>,
    delay: Duration,
    done: bool,
}

impl StreamSubscription<'_> {
    /// Pause before reconnecting after the server closes a connection
    /// (default one second).
    pub fn with_reconnect_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Stop the subscription: `next` returns `None` from now on.
    pub fn close(&mut self) {
        self.done = true;
        self.stream = None;
    }

    /// Yield the next event. `Some(Err(_))` ends the subscription (for example
    /// `K9O_STREAM_UNAVAILABLE` while reconnecting); `None` once closed.
    pub async fn next(&mut self) -> Option<Result<StreamEvent, KError>> {
        loop {
            if self.done {
                return None;
            }
            let Some(stream) = self.stream.as_mut() else {
                tokio::time::sleep(self.delay).await;
                match open(self.config, &self.path, self.last.as_deref()).await {
                    Ok(s) => self.stream = Some(s),
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
                continue;
            };
            match stream.next().await {
                Some(Ok(ev)) => {
                    self.last = Some(ev.sequence.clone());
                    return Some(Ok(ev));
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                None => self.stream = None,
            }
        }
    }
}

async fn open(
    config: &HttpClientConfig,
    path: &str,
    from: Option<&str>,
) -> Result<SseStream<StreamEvent>, KError> {
    let q: Vec<(String, String)> = from
        .map(|s| vec![("fromSequence".to_string(), s.to_string())])
        .unwrap_or_default();
    open_sse_stream::<StreamEvent, ()>(config, Method::GET, path, None, Some(&q)).await
}

/// Streams namespace — `client.streams()`.
pub struct StreamsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> StreamsNamespace<'a> {
    /// Create a user stream: `POST /@<slug>/data-streams/user`. `max_bytes` is
    /// the storage ceiling; `retention_seconds` is optional (the gateway
    /// applies its default).
    pub async fn create(
        &self,
        display_name: &str,
        max_bytes: i64,
        retention_seconds: Option<i64>,
    ) -> Result<Response<Stream>, KError> {
        let slug = require_project(self.config, "streams")?;
        let mut body = json!({ "displayName": display_name, "maxBytes": max_bytes });
        if let Some(r) = retention_seconds {
            body["retentionSeconds"] = json!(r);
        }
        request::<Stream, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "data-streams/user"),
            Some(&body),
            None,
        )
        .await
    }

    /// List the project's user streams.
    pub async fn list(&self) -> Result<Response<Vec<Stream>>, KError> {
        let slug = require_project(self.config, "streams")?;
        request::<Vec<Stream>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "data-streams/user"),
            None,
            None,
        )
        .await
    }

    /// Get one stream by slug.
    pub async fn get(&self, stream_slug: &str) -> Result<Response<Stream>, KError> {
        let slug = require_project(self.config, "streams")?;
        request::<Stream, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("data-streams/user/{}", enc(stream_slug))),
            None,
            None,
        )
        .await
    }

    /// Retire a stream.
    pub async fn delete(&self, stream_slug: &str) -> Result<Response<StreamDeleted>, KError> {
        let slug = require_project(self.config, "streams")?;
        request::<StreamDeleted, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("data-streams/user/{}", enc(stream_slug))),
            None,
            None,
        )
        .await
    }

    /// Append an event. `data` is any JSON-serialisable value; `subject` is an
    /// optional dot-separated routing suffix. Needs project admin.
    pub async fn publish<T: Serialize + ?Sized>(
        &self,
        stream_slug: &str,
        data: &T,
        subject: Option<&str>,
    ) -> Result<Response<PublishResult>, KError> {
        let slug = require_project(self.config, "streams")?;
        let mut body = json!({ "data": serde_json::to_value(data).map_err(|e| {
            KError::new("K9O_INVALID_FIELD_VALUE", format!("data is not serialisable: {}", e))
        })? });
        if let Some(s) = subject {
            body["subject"] = json!(s);
        }
        request::<PublishResult, Value>(
            self.config,
            Method::POST,
            &project_path(
                slug,
                &format!("data-streams/user/{}/publish", enc(stream_slug)),
            ),
            Some(&body),
            None,
        )
        .await
    }

    /// Read a bounded range of events in one call. Needs project admin or auditor.
    pub async fn replay(
        &self,
        stream_slug: &str,
        from_sequence: Option<u64>,
        until_sequence: Option<u64>,
        limit: Option<u32>,
    ) -> Result<Response<ReplayResult>, KError> {
        let slug = require_project(self.config, "streams")?;
        let mut body = json!({});
        if let Some(v) = from_sequence {
            body["fromSequence"] = json!(v);
        }
        if let Some(v) = until_sequence {
            body["untilSequence"] = json!(v);
        }
        if let Some(v) = limit {
            body["limit"] = json!(v);
        }
        request::<ReplayResult, Value>(
            self.config,
            Method::POST,
            &project_path(
                slug,
                &format!("data-streams/user/{}/replay", enc(stream_slug)),
            ),
            Some(&body),
            None,
        )
        .await
    }

    /// Tail a stream over SSE: `GET .../subscribe`. `from_sequence` resumes
    /// after that sequence (`None` starts at the beginning). The first
    /// connection is opened here, so a refusal such as `K9O_STREAM_UNAVAILABLE`
    /// is returned as `Err`. Needs project admin or auditor.
    pub async fn subscribe(
        &self,
        stream_slug: &str,
        from_sequence: Option<&str>,
    ) -> Result<StreamSubscription<'a>, KError> {
        let slug = require_project(self.config, "streams")?;
        let path = project_path(
            slug,
            &format!("data-streams/user/{}/subscribe", enc(stream_slug)),
        );
        let stream = open(self.config, &path, from_sequence).await?;
        Ok(StreamSubscription {
            config: self.config,
            path,
            stream: Some(stream),
            last: from_sequence.map(str::to_string),
            delay: Duration::from_secs(1),
            done: false,
        })
    }
}
