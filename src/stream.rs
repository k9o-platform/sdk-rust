//! SSE streaming support for the k9o SDK — internal.
//!
//! Used by Compute LLMs `invoke_stream` + Realtime
//! `subscribe` SSE transport. The Rust
//! async-iteration surface is an explicit `next().await` loop over a
//! stream handle (dependency-free; no `futures::Stream` machinery), e.g.
//!
//! ```ignore
//! let mut stream = client.compute().llms().invoke_stream("gpt-4o", input).await?;
//! while let Some(chunk) = stream.next().await { let chunk = chunk?; /* ... */ }
//! ```

use crate::http::HttpClientConfig;
use crate::kerror::{KError, KErrorWire};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::marker::PhantomData;

/// Require a bound project slug or return K9O_MISSING_PROJECT_CONTEXT.
pub(crate) fn require_project<'a>(
    config: &'a HttpClientConfig,
    surface: &str,
) -> Result<&'a str, KError> {
    config.project_slug.as_deref().ok_or_else(|| {
        KError::new(
            "K9O_MISSING_PROJECT_CONTEXT",
            format!(
                "{} operations require project_slug bound on the client",
                surface
            ),
        )
    })
}

/// An SSE stream handle yielding decoded `T` payloads. Construct via
/// [`open_sse_stream`]; iterate with `next().await`.
pub struct SseStream<T> {
    response: reqwest::Response,
    buffer: String,
    done: bool,
    _marker: PhantomData<T>,
}

impl<T: DeserializeOwned> SseStream<T> {
    /// Yield the next decoded event, or `None` when the stream ends
    /// (`[DONE]` sentinel or EOF). `Some(Err(_))` on a transport/decode
    /// error; the stream is exhausted afterward.
    pub async fn next(&mut self) -> Option<Result<T, KError>> {
        if self.done {
            return None;
        }
        loop {
            // Try to extract a complete event from the buffer first.
            if let Some(idx) = find_boundary(&self.buffer) {
                let (raw, advance) = split_event(&self.buffer, idx);
                self.buffer.drain(..advance);
                match extract_data(&raw) {
                    None => continue,
                    Some(payload) if payload == "[DONE]" => {
                        self.done = true;
                        return None;
                    }
                    Some(payload) => match serde_json::from_str::<T>(&payload) {
                        Ok(v) => return Some(Ok(v)),
                        Err(e) => {
                            self.done = true;
                            return Some(Err(KError::new(
                                "K9O_INTERNAL_ERROR",
                                format!("failed to decode stream chunk: {}", e),
                            )));
                        }
                    },
                }
            }

            // Need more bytes from the network.
            match self.response.chunk().await {
                Ok(Some(bytes)) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Ok(None) => {
                    // EOF — flush any trailing event without a boundary.
                    self.done = true;
                    let tail = self.buffer.trim().to_string();
                    self.buffer.clear();
                    if let Some(payload) = extract_data(&tail) {
                        if payload != "[DONE]" {
                            return Some(serde_json::from_str::<T>(&payload).map_err(|e| {
                                KError::new(
                                    "K9O_INTERNAL_ERROR",
                                    format!("failed to decode stream chunk: {}", e),
                                )
                            }));
                        }
                    }
                    return None;
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(KError::new(
                        "K9O_UPSTREAM_UNAVAILABLE",
                        format!("stream read error: {}", e),
                    )));
                }
            }
        }
    }
}

/// Send the SSE request and return the raw response on 2xx; `Err(KError)` on
/// non-2xx (KError wire shape) or transport error. `extra_headers` are added
/// after the defaults.
pub(crate) async fn open_sse_response<B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
    extra_headers: &[(&str, &str)],
) -> Result<reqwest::Response, KError> {
    let origin = crate::region::resolve_origin(&config.base_url, &config.region).await;
    let url = format!("{}{}", origin, path);
    let mut req = config
        .reqwest
        .request(method, &url)
        .header("accept", "text/event-stream");
    for (k, v) in &config.default_headers {
        req = req.header(k, v);
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(slug) = &config.project_slug {
        req = req.header("x-k9o-project-slug", slug);
    }
    if let Some(slug) = &config.org_slug {
        req = req.header("x-k9o-org-slug", slug);
    }
    if let Some(token) = &config.api_key {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    if let Some(q) = query {
        req = req.query(q);
    }

    let response = req.send().await.map_err(|e| {
        KError::new(
            "K9O_UPSTREAM_UNAVAILABLE",
            format!("Network error reaching {}: {}", url, e),
        )
        .with_context("url", url.clone())
    })?;

    let status = response.status();
    if !status.is_success() {
        let resp_headers = response.headers().clone();
        let bytes = response.bytes().await.unwrap_or_default();
        if let Ok(wire) = serde_json::from_slice::<KErrorWire>(&bytes) {
            return Err(crate::kerror::fill_request_id(
                wire.into_kerror(status.as_u16()),
                &resp_headers,
            ));
        }
        return Err(crate::kerror::fill_request_id(
            KError::new(
                "K9O_INTERNAL_ERROR",
                format!("Upstream returned {} opening stream", status.as_u16()),
            )
            .with_context("url", url)
            .with_context("status", status.as_u16()),
            &resp_headers,
        ));
    }
    Ok(response)
}

/// Open an SSE stream against the gateway. Returns the stream handle on
/// 2xx; `Err(KError)` on non-2xx (KError wire shape) or transport error.
pub(crate) async fn open_sse_stream<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
) -> Result<SseStream<T>, KError> {
    let response = open_sse_response(config, method, path, body, query, &[]).await?;
    Ok(SseStream {
        response,
        buffer: String::new(),
        done: false,
        _marker: PhantomData,
    })
}

/// Index of the end of the first complete event block (position of the
/// blank-line separator), or None if no complete event yet.
fn find_boundary(buf: &str) -> Option<usize> {
    let lf = buf.find("\n\n");
    let crlf = buf.find("\r\n\r\n");
    match (lf, crlf) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(a.min(b)),
    }
}

/// Returns (event_block, bytes_to_drain_including_separator).
fn split_event(buf: &str, idx: usize) -> (String, usize) {
    let raw = buf[..idx].to_string();
    // Drain past the separator (handles both \n\n and \r\n\r\n).
    let rest = &buf[idx..];
    let sep_len = if rest.starts_with("\r\n\r\n") { 4 } else { 2 };
    (raw, idx + sep_len)
}

/// Extract + concatenate `data:` lines from a raw SSE event block.
fn extract_data(raw: &str) -> Option<String> {
    let mut data_lines: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}
