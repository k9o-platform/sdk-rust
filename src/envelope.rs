//! Envelope + meta namespace types.
//!
//! `Response<T>` is the public name (matches spec sample
//! `Result<Response<T>, KError>`); `Envelope<T>` is the internal alias.
//! `serde(rename_all = "camelCase")` translates wire camelCase ↔ Rust
//! snake_case at the boundary invariant.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The standard envelope returned by every k9o surface on
/// success. Generic over the per-method data shape.
///
/// Customer-facing alias: `Response<T>` (matches spec sample).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Envelope<T> {
    pub data: T,
    #[serde(default)]
    pub meta: Meta,
}

/// Type alias `Response<T>` = `Envelope<T>` spec sample.
pub type Response<T> = Envelope<T>;

/// Structured meta accessor sugar Sub-namespaces are
/// `Option<T>` — `None` when the gateway didn't send the corresponding
/// sub-namespace; `Some(...)` when present.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Meta {
    #[serde(default)]
    pub cursor: Option<MetaCursor>,
    #[serde(default)]
    pub subscription: Option<MetaSubscription>,
    #[serde(default)]
    pub idempotency: Option<MetaIdempotency>,
    #[serde(default)]
    pub warnings: Vec<MetaWarning>,
    /// Open value: the gateway emits `meta.cost` as a sparse
    /// structured object (e.g. `{"egress":{"gb":4.55e-07,"zone":"intra-region"}}`),
    /// not a scalar. Modeled as `serde_json::Value` so future cost
    /// dimensions deserialize without a schema change. Absent → `Value::Null`.
    #[serde(default)]
    pub cost: Value,
    #[serde(default)]
    pub timing: Option<MetaTiming>,
    #[serde(default)]
    pub context: Option<MetaContext>,
    /// Present on `client.query` responses.
    #[serde(rename = "rawQuery", default)]
    pub raw_query: Option<MetaRawQuery>,
    /// Present when a write was queued for approval instead of applied.
    #[serde(default)]
    pub approval: Option<MetaApproval>,
}

/// Approval state of a queued write: repeats `data.status`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaApproval {
    pub status: String,
}

/// What `client.query` ran: the statement's intent and the number of rows returned.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaRawQuery {
    pub intent: String,
    #[serde(rename = "rowCount")]
    pub row_count: u64,
}

/// Pagination cursor
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaCursor {
    pub next: Option<String>,
    #[serde(rename = "totalCount", default)]
    pub total_count: Option<u64>,
}

/// Subscription delivery metadata
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaSubscription {
    #[serde(rename = "isInitial")]
    pub is_initial: bool,
}

/// Idempotency replay flag
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaIdempotency {
    pub replayed: bool,
}

/// Non-fatal warning
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaWarning {
    pub code: String,
    pub message: String,
}

/// Wall-clock timing
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaTiming {
    /// Fractional (sub-ms) — the gateway emits e.g. 10.572.
    #[serde(rename = "totalMs")]
    pub total_ms: f64,
}

/// Request-correlation metadata
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaContext {
    #[serde(rename = "requestId")]
    pub request_id: String,
}
