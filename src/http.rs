//! HTTP client for the k9o SDK — reqwest async.
//!
//! Rust idiomatic shape: returns `Result<Response<T>,
//! KError>`. Generic over the data shape via serde::Deserialize on T.

use crate::envelope::{Envelope, Response};
use crate::kerror::{KError, KErrorWire};
use reqwest::{Client as ReqwestClient, Method};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// Backoff strategy for retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    Exponential,
    Linear,
    None,
}

/// Retry + backoff policy. Applied to idempotent methods
/// (GET/HEAD/OPTIONS/DELETE) on transient failures only (transport error /
/// 502 / 503 / 504 / 429); semantic 4xx + non-idempotent methods are never
/// retried.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub backoff: Backoff,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff: Backoff::Exponential,
            initial_delay_ms: 100,
            max_delay_ms: 5000,
        }
    }
}

/// Per-instance HTTP config.
#[derive(Debug, Clone)]
pub(crate) struct HttpClientConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub org_slug: Option<String>,
    pub project_slug: Option<String>,
    pub default_headers: HashMap<String, String>,
    pub reqwest: Arc<ReqwestClient>,
    pub retry: RetryConfig,
    /// Regional host resolution state (probe transport + cached origin).
    pub region: Arc<crate::region::RegionState>,
    /// Cached key feed and project id for Rampart receipts.
    pub rampart: Arc<crate::rampart::RampartState>,
    /// The registered crypto client core.
    pub crypto: Arc<crate::crypto::CryptoState>,
    /// Checked `expected_key_refs`, passed to the client core on every call.
    pub expected_key_refs: Option<Vec<String>>,
}

fn is_retryable(err: &KError, method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::DELETE
    ) && (err.code == "K9O_UPSTREAM_UNAVAILABLE"
        || matches!(err.status, Some(502 | 503 | 504 | 429)))
}

/// Cheap, non-cryptographic jitter factor in [-1.0, 1.0) from system-time
/// nanos — retry jitter does not need a strong RNG, and this avoids a
/// `rand` dependency.
fn jitter_factor() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 500.0 - 1.0
}

/// Backoff delay (ms) for a 0-based attempt index, capped + ±25% jittered.
fn compute_delay_ms(cfg: &RetryConfig, attempt: u32) -> u64 {
    if cfg.backoff == Backoff::None {
        return 0;
    }
    let base = match cfg.backoff {
        Backoff::Linear => cfg.initial_delay_ms.saturating_mul(attempt as u64 + 1),
        _ => cfg
            .initial_delay_ms
            .saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX)),
    };
    let capped = base.min(cfg.max_delay_ms);
    let jittered = capped as f64 + capped as f64 * 0.25 * jitter_factor();
    jittered.max(0.0) as u64
}

/// Issue a request against the gateway, with retry on transient failures.
///
/// Idempotent methods are retried with backoff per `config.retry`
///; semantic / non-idempotent failures return immediately.
/// Generic on `T` (response data shape) + `B` (body shape).
pub(crate) async fn request<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
) -> Result<Response<T>, KError> {
    request_with_headers::<T, B>(config, method, path, body, query, None).await
}

/// Like [`request`], plus caller-supplied per-request headers (e.g. the
/// vault-secrets `X-K9O-Reveal-Challenge` header on `reveal`).
pub(crate) async fn request_with_headers<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
    headers: Option<&[(String, String)]>,
) -> Result<Response<T>, KError> {
    request_inner::<T, B>(
        config,
        method,
        path,
        body,
        query,
        headers,
        None,
        false,
        Target::Base,
    )
    .await
}

/// Like [`request`] for an answer that is not wrapped in `{ data, meta }`: `T` is read from the
/// whole body.
pub(crate) async fn request_bare<T: DeserializeOwned>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
) -> Result<Response<T>, KError> {
    request_inner::<T, ()>(
        config,
        method,
        path,
        None,
        None,
        None,
        None,
        true,
        Target::Base,
    )
    .await
}

/// Like [`request_bare`], with a query string.
pub(crate) async fn request_bare_query<T: DeserializeOwned>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    query: Option<&[(String, String)]>,
) -> Result<Response<T>, KError> {
    request_inner::<T, ()>(
        config,
        method,
        path,
        None,
        query,
        None,
        None,
        true,
        Target::Base,
    )
    .await
}

/// A pre-encoded request body with its own `content-type` (multipart upload).
/// The content type is set verbatim (it carries the boundary); the JSON
/// serialiser is not involved.
pub(crate) struct RawBody {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Like [`request`], but sends `raw` as the body instead of JSON.
pub(crate) async fn request_raw<T: DeserializeOwned>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    raw: RawBody,
) -> Result<Response<T>, KError> {
    request_inner::<T, ()>(
        config,
        method,
        path,
        None,
        None,
        None,
        Some(&raw),
        false,
        Target::Base,
    )
    .await
}

/// Test-only: maps a logical origin (an `https` gateway host that cannot be served locally) to the address of a local
/// test server, so a test can see which origin a request was meant for.
#[cfg(test)]
pub(crate) mod test_routes {
    use std::collections::HashMap;
    use std::sync::Mutex;

    static ROUTES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

    pub(crate) fn route(logical: &str, physical: &str) {
        ROUTES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(logical.to_string(), physical.to_string());
    }

    pub(crate) fn physical(origin: &str) -> String {
        ROUTES
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|m| m.get(origin).cloned())
            .unwrap_or_else(|| origin.to_string())
    }
}

/// Where a request goes and what it carries.
#[derive(Clone, Copy)]
pub(crate) enum Target<'a> {
    /// The client's regional base, with the client's credentials.
    Base,
    /// A specific origin (the credential flows, after the locate rule), with the client's credentials.
    Origin(&'a str),
    /// The client's regional base, with no `Authorization` and none of the default headers.
    Anonymous,
}

/// POST `path` to `target`. `Target::Anonymous` is for the locate door: no credential of any kind is sent.
pub(crate) async fn request_to<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    target: Target<'_>,
) -> Result<Response<T>, KError> {
    request_inner::<T, B>(config, method, path, body, None, None, None, false, target).await
}

async fn request_inner<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
    headers: Option<&[(String, String)]>,
    raw: Option<&RawBody>,
    bare: bool,
    target: Target<'_>,
) -> Result<Response<T>, KError> {
    let cfg = &config.retry;
    let mut attempt: u32 = 0;
    loop {
        match request_once::<T, B>(
            config,
            method.clone(),
            path,
            body,
            query,
            headers,
            raw,
            bare,
            target,
        )
        .await
        {
            Ok(r) => return Ok(r),
            Err(e) => {
                if attempt >= cfg.max_retries || !is_retryable(&e, &method) {
                    return Err(e);
                }
                let delay = compute_delay_ms(cfg, attempt);
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                attempt += 1;
            }
        }
    }
}

/// Single-attempt request. `request`/`request_with_headers` wrap this in
/// retry-with-backoff.
async fn request_once<T: DeserializeOwned, B: Serialize + ?Sized>(
    config: &HttpClientConfig,
    method: Method,
    path: &str,
    body: Option<&B>,
    query: Option<&[(String, String)]>,
    headers: Option<&[(String, String)]>,
    raw: Option<&RawBody>,
    bare: bool,
    target: Target<'_>,
) -> Result<Response<T>, KError> {
    let origin = match target {
        Target::Origin(o) => o.to_string(),
        _ => crate::region::resolve_origin(&config.base_url, &config.region).await,
    };
    #[cfg(test)]
    let origin = test_routes::physical(&origin);
    let anonymous = matches!(target, Target::Anonymous);
    let url = format!("{}{}", origin, path);

    let mut req = config.reqwest.request(method, &url);
    req = req.header("accept", "application/json");
    if !anonymous {
        for (k, v) in &config.default_headers {
            req = req.header(k, v);
        }
    }
    if let Some(slug) = &config.project_slug {
        req = req.header("x-k9o-project-slug", slug);
    }
    if let Some(slug) = &config.org_slug {
        req = req.header("x-k9o-org-slug", slug);
    }
    if let Some(token) = config.api_key.as_ref().filter(|_| !anonymous) {
        req = req.header("authorization", format!("Bearer {}", token));
    }
    if let Some(hs) = headers {
        for (k, v) in hs {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    if let Some(r) = raw {
        req = req
            .header("content-type", r.content_type.as_str())
            .body(r.bytes.clone());
    } else if let Some(b) = body {
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
        .with_recommendation("Check connectivity to the gateway; retry with backoff if transient.")
    })?;

    let status = response.status();
    let retry_after_header = retry_after_secs(response.headers());
    let resp_headers = response.headers().clone();

    // 204 No Content
    if status == 204 {
        // Synthesize a minimal envelope. Caller's T must be deserializable
        // from null + empty meta.
        let bytes = b"{\"data\":null,\"meta\":{}}";
        let env: Envelope<T> = serde_json::from_slice(bytes).map_err(|e| {
            KError::new(
                "K9O_INTERNAL_ERROR",
                format!("204 No Content but T cannot deserialize null: {}", e),
            )
        })?;
        return Ok(env);
    }

    let bytes = response.bytes().await.map_err(|e| {
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!("Failed to read response body from {}: {}", url, e),
        )
    })?;

    if status.is_success() {
        // A bare answer has no `data` wrapper (the key feed is `{ keys, meta }`): the whole
        // body is the data and its own `meta`, when present, is the envelope meta.
        let bytes = if bare {
            let whole: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                KError::new(
                    "K9O_INTERNAL_ERROR",
                    format!("Malformed response from {}: {}", url, e),
                )
                .with_context("url", url.clone())
                .with_context("status", status.as_u16())
            })?;
            let meta = whole
                .get("meta")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            serde_json::to_vec(&serde_json::json!({ "data": whole, "meta": meta }))
                .unwrap_or_default()
                .into()
        } else {
            bytes
        };
        let env: Envelope<T> = serde_json::from_slice(&bytes).map_err(|e| {
            KError::new(
                "K9O_INTERNAL_ERROR",
                format!("Malformed envelope from {}: {}", url, e),
            )
            .with_context("url", url.clone())
            .with_context("status", status.as_u16())
        })?;
        return Ok(env);
    }

    // Non-2xx — try KError wire shape
    if let Ok(wire) = serde_json::from_slice::<KErrorWire>(&bytes) {
        let mut err = wire.into_kerror(status.as_u16());
        if err.retry_after_sec.is_none() {
            err.retry_after_sec = retry_after_header;
        }
        return Err(crate::kerror::fill_request_id(err, &resp_headers));
    }
    Err(crate::kerror::fill_request_id(
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!("Upstream returned {} with non-KError body", status.as_u16()),
        )
        .with_context("url", url)
        .with_context("status", status.as_u16()),
        &resp_headers,
    ))
}

/// The `Retry-After` header as a number of seconds, when it is a finite number of at least zero.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<f64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// A binary answer: the bytes and the `content-type` the gateway sent.
pub(crate) struct Download {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// GET `path` and return the body as bytes (not a JSON envelope). Non-2xx answers are decoded as
/// a `KError`. Retried like any other GET.
pub(crate) async fn request_download(
    config: &HttpClientConfig,
    path: &str,
) -> Result<Download, KError> {
    let cfg = &config.retry;
    let mut attempt: u32 = 0;
    loop {
        match download_once(config, path).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                if attempt >= cfg.max_retries || !is_retryable(&e, &Method::GET) {
                    return Err(e);
                }
                let delay = compute_delay_ms(cfg, attempt);
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                attempt += 1;
            }
        }
    }
}

async fn download_once(config: &HttpClientConfig, path: &str) -> Result<Download, KError> {
    let origin = crate::region::resolve_origin(&config.base_url, &config.region).await;
    let url = format!("{}{}", origin, path);
    let mut req = config.reqwest.request(Method::GET, &url);
    for (k, v) in &config.default_headers {
        req = req.header(k, v);
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
    let response = req.send().await.map_err(|e| {
        KError::new(
            "K9O_UPSTREAM_UNAVAILABLE",
            format!("Network error reaching {}: {}", url, e),
        )
        .with_context("url", url.clone())
    })?;
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let retry_after_header = retry_after_secs(response.headers());
    let resp_headers = response.headers().clone();
    let bytes = response.bytes().await.map_err(|e| {
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!("Failed to read response body from {}: {}", url, e),
        )
    })?;
    if status.is_success() {
        return Ok(Download {
            content_type,
            bytes: bytes.to_vec(),
        });
    }
    if let Ok(wire) = serde_json::from_slice::<KErrorWire>(&bytes) {
        let mut err = wire.into_kerror(status.as_u16());
        if err.retry_after_sec.is_none() {
            err.retry_after_sec = retry_after_header;
        }
        return Err(crate::kerror::fill_request_id(err, &resp_headers));
    }
    Err(crate::kerror::fill_request_id(
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!("Upstream returned {} with non-KError body", status.as_u16()),
        )
        .with_context("url", url)
        .with_context("status", status.as_u16()),
        &resp_headers,
    ))
}
