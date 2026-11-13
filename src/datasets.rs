//! Datasets: `client.analytics()` and `client.rollups()`.
//!
//! Wire mapping (all under `/@<slug>`):
//!   - `analytics().list / query`                    -> `GET /analytics`, `POST /analytics/query`
//!   - `analytics().create / get / delete`           -> `POST|GET|DELETE /analytics/<dataset>`
//!   - `analytics().materialize / read / write`      -> `POST /analytics/<dataset>/{materialize,read,write}`
//!   - `analytics().upload / download_original`      -> `POST .../upload`, `GET .../upload/original`
//!   - `analytics().rollups()`                       -> `POST|GET .../rollups`, `DELETE .../rollups/<id>`,
//!                                                       `POST .../rollups/<id>/reattest`
//!   - `analytics().schedules()`                     -> the same four calls under `.../schedules`
//!   - `rollups().list / read / subscribe`           -> `GET /rollups`, `GET /rollups/<slug>/read`,
//!                                                       `GET /rollups/<slug>/subscribe` (SSE)
//!
//! `reattest` is for a person only: a service, workload or extension token gets 403 `K9O_RBAC_DENY` with
//! `reason: human_only`. A rollup or schedule that is not re-attested within its `reattest_interval_days` pauses.

use crate::envelope::Response;
use crate::http::{request, request_download, request_raw, HttpClientConfig, RawBody};
use crate::kerror::KError;
use crate::realtime::{open_feed_kind, FeedKind, SubscribeOptions, Subscription};
use crate::storage::{boundary, multipart_body};
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// A list answer. The gateway sends either a JSON array or an object holding one array (for example
/// `{ "rollups": [...] }`); both read as the list. It derefs to a slice, so `list.len()`,
/// `list[0]` and `for x in &list` work.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Listed<T>(pub Vec<T>);

impl<T> std::ops::Deref for Listed<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> IntoIterator for Listed<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Listed<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for Listed<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = Value::deserialize(d)?;
        let list = match v {
            Value::Array(a) => a,
            Value::Object(m) => {
                let mut arrays = m.into_iter().filter_map(|(_, v)| match v {
                    Value::Array(a) => Some(a),
                    _ => None,
                });
                match (arrays.next(), arrays.next()) {
                    (Some(a), None) => a,
                    (None, _) => Vec::new(),
                    _ => return Err(D::Error::custom("expected a list")),
                }
            }
            _ => return Err(D::Error::custom("expected a list")),
        };
        list.into_iter()
            .map(|e| serde_json::from_value(e).map_err(D::Error::custom))
            .collect::<Result<Vec<T>, _>>()
            .map(Listed)
    }
}

/// Largest upload the gateway accepts.
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
const REATTEST_DAYS: [u16; 4] = [7, 30, 90, 365];

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

/// A path segment: not empty, not a dot segment, percent-encoded.
fn seg(what: &str, value: &str) -> Result<String, KError> {
    if value.is_empty() || value == "." || value == ".." {
        return Err(invalid(format!("{what} must be a non-empty identifier")));
    }
    Ok(enc(value))
}

fn check_reattest(days: Option<u16>) -> Result<(), KError> {
    match days {
        Some(d) if !REATTEST_DAYS.contains(&d) => {
            Err(invalid("reattest_interval_days must be 7, 30, 90 or 365"))
        }
        _ => Ok(()),
    }
}

/// How a dataset's rows are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResidencyClass {
    /// Rows belong to one data subject each and stay in that subject's region.
    #[serde(rename = "per-subject")]
    PerSubject,
    /// Rows hold no per-subject personal data.
    #[serde(rename = "shared")]
    Shared,
}

/// Where a dataset's files live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StorageTarget {
    /// The platform's storage.
    Managed,
    /// Your own storage binding.
    Byos {
        #[serde(rename = "bindingId")]
        binding_id: String,
    },
}

/// One column of a dataset: `type` is `string`, `int32`, `int64`, `float64`, `bool`, `date`,
/// `timestamp`, `json`, `bytes` or `decimal(p,s)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub column_type: String,
}

impl DatasetColumn {
    pub fn new(name: impl Into<String>, column_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            column_type: column_type.into(),
        }
    }
}

/// Input of `analytics().create`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDatasetInput {
    /// 1 to 200 characters.
    pub display_name: String,
    /// 1 to 512 columns.
    pub columns: Vec<DatasetColumn>,
    /// Up to 16 partition keys. A timestamp key needs a transform such as `created_at:day`;
    /// `region` is added by the platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_by: Option<Vec<String>>,
    pub residency_class: ResidencyClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_target: Option<StorageTarget>,
}

/// A dataset. Members the gateway adds later are in `extra`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dataset {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub residency_class: Option<String>,
    #[serde(default)]
    pub storage_target: Option<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The answer of `analytics().delete`: `{ slug, status: "retired" }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatasetRetired {
    pub slug: String,
    pub status: String,
}

/// What `write` and `upload` report.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResult {
    #[serde(default)]
    pub ingested: Option<u64>,
    #[serde(default)]
    pub skipped: Option<u64>,
    /// Up to 100 rows that were skipped and why.
    #[serde(default)]
    pub skipped_details: Vec<Value>,
    /// The rest of the answer: the partitions touched and the bytes written (and, for a dry run, the
    /// detected columns and a 50-row preview).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What `materialize` reports.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterializeResult {
    #[serde(default)]
    pub rows_read: Option<u64>,
    #[serde(default)]
    pub ingested: Option<u64>,
    #[serde(default)]
    pub rejected: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// What to do with a row that fails the dataset's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Skip bad rows and report them (the default).
    Skip,
    /// Refuse the whole write (400 `K9O_INGEST_SCHEMA_MISMATCH`).
    Reject,
}

impl OnError {
    fn as_str(self) -> &'static str {
        match self {
            OnError::Skip => "skip",
            OnError::Reject => "reject",
        }
    }
}

/// Options of `analytics().write`.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    pub on_error: Option<OnError>,
}

/// File format of an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadFormat {
    Csv,
    Json,
}

impl UploadFormat {
    fn as_str(self) -> &'static str {
        match self {
            UploadFormat::Csv => "csv",
            UploadFormat::Json => "json",
        }
    }
}

/// Options of `analytics().upload`.
#[derive(Debug, Clone)]
pub struct UploadOptions {
    pub format: UploadFormat,
    /// Detect the columns and return a 50-row preview without writing.
    pub dry_run: bool,
    pub on_error: Option<OnError>,
    /// CSV only.
    pub delimiter: Option<String>,
    /// CSV only.
    pub encoding: Option<String>,
    /// CSV only.
    pub has_header: Option<bool>,
    /// A manifest on a slug that does not exist yet creates the dataset and loads the file in one call
    /// (same members as [`CreateDatasetInput`]). Sent as a multipart field.
    pub manifest: Option<Value>,
    /// The file name sent with a multipart upload.
    pub file_name: Option<String>,
}

impl UploadOptions {
    pub fn new(format: UploadFormat) -> Self {
        Self {
            format,
            dry_run: false,
            on_error: None,
            delimiter: None,
            encoding: None,
            has_header: None,
            manifest: None,
            file_name: None,
        }
    }
}

/// The original file of an upload.
#[derive(Debug, Clone)]
pub struct OriginalFile {
    /// `text/csv`, `application/json` or `application/octet-stream`.
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// Options of `analytics().read`.
#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    /// Constraints on declared partition keys.
    pub partition_filter: Option<Value>,
    /// `meta.cursor.next` of the previous page.
    pub cursor: Option<String>,
    /// Page size; the gateway clamps it.
    pub limit: Option<u32>,
}

/// An aggregate over a rollup's stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RollupMeasure {
    pub name: String,
    /// `count`, or `last`, `sum`, `avg`, `min`, `max`, `count_distinct`, `p50`, `p95`, `p99`.
    pub agg: String,
    /// The column, for every aggregate but `count`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
}

/// Input of `analytics().rollups().create`. Give exactly one of `subject_field` and `non_personal`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRollupInput {
    /// 1 to 63 characters.
    pub slug: String,
    pub source: String,
    /// Up to 16.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<String>,
    /// 1 to 64.
    pub measures: Vec<RollupMeasure>,
    /// At least 60.
    pub window_seconds: u32,
    /// 7, 30, 90 or 365 (the gateway default is 90).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattest_interval_days: Option<u16>,
    /// Per-subject data: cells below the cohort threshold are suppressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_field: Option<String>,
    /// The rollup holds no personal data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_personal: Option<bool>,
}

/// A continuous rollup. `attestation_status` is `active`, `expiring_soon` or `expired` (an expired
/// rollup is paused, not deleted). The read and subscribe endpoints are in `extra`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rollup {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub attested_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub attestation_status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Input of `analytics().schedules().create`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDatasetScheduleInput {
    pub from_collection: String,
    /// At least 60.
    pub interval_seconds: u32,
    /// 7, 30, 90 or 365.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reattest_interval_days: Option<u16>,
}

/// A recurring materialization into a dataset.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetSchedule {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_run_status: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<String>,
    #[serde(default)]
    pub attested_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub attestation_status: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The answer of a rollup or schedule delete: `{ id, deleted: true }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeletedRef {
    pub id: String,
    pub deleted: bool,
}

/// Options of `rollups().read`.
#[derive(Debug, Clone, Default)]
pub struct RollupReadOptions {
    /// ISO 8601; default the last 24 hours (at most 50,000 cells in the range).
    pub from: Option<String>,
    pub to: Option<String>,
    /// A projection of dimensions. Only `count`, `sum`, `min` and `max` measures combine.
    pub dims: Option<Vec<String>>,
}

/// Options of `rollups().subscribe`.
#[derive(Clone, Default)]
pub struct RollupSubscribeOptions {
    /// Start from this feed position instead of from now. First connection only.
    pub from_sequence: Option<u64>,
    pub feed: SubscribeOptions,
}

/// One cell of a rollup's stream.
#[derive(Debug, Clone, PartialEq)]
pub struct RollupCell {
    /// The frame's `eventId`. Delivery is at least once (a resume re-sends the last 5 seconds); the
    /// subscription drops repeats by it.
    pub event_id: String,
    pub topic: String,
    pub occurred_at: String,
    pub cell_key: String,
    pub bucket: String,
    /// The dimension values of the cell (absent on a suppressed cell).
    pub dims: Value,
    /// True for a closed cell.
    pub is_final: bool,
    /// The measures of the cell; `None` on a suppressed cell.
    pub measures: Option<Value>,
    /// `Some("suppressed")` for a cell below the cohort threshold; `None` otherwise.
    pub status: Option<String>,
    /// Why a cell is suppressed, for example `cohort_below_k`.
    pub reason: Option<String>,
    /// The whole `payload` of the frame, unknown fields included.
    pub raw: Value,
}

impl RollupCell {
    /// True when the cell is withheld because too few subjects fall in it.
    pub fn is_suppressed(&self) -> bool {
        self.status.as_deref() == Some("suppressed")
    }
}

/// A live rollup stream: [`RollupCell`]s from `rollups().subscribe`. It is the change feed of
/// `realtime().subscribe` (reconnects with `Last-Event-ID`, drops repeats, ignores heartbeats).
pub struct RollupSubscription {
    inner: Subscription,
}

impl RollupSubscription {
    /// The next cell, or `None` once closed. `Some(Err(_))` ends the stream.
    pub async fn next(&mut self) -> Option<Result<RollupCell, KError>> {
        loop {
            let ev = match self.inner.next().await? {
                Ok(ev) => ev,
                Err(e) => return Some(Err(e)),
            };
            let p = &ev.payload;
            let text = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
            let Some(cell_key) = text("cellKey") else {
                continue;
            };
            return Some(Ok(RollupCell {
                event_id: ev.event_id.clone(),
                topic: ev.topic.clone(),
                occurred_at: ev.occurred_at.clone(),
                cell_key,
                bucket: text("bucket").unwrap_or_default(),
                dims: p.get("dims").cloned().unwrap_or(Value::Null),
                is_final: p.get("final").and_then(Value::as_bool).unwrap_or(false),
                measures: p.get("measures").cloned(),
                status: text("status"),
                reason: text("reason"),
                raw: ev.payload,
            }));
        }
    }

    /// Stop the stream: `next` returns `None` from now on.
    pub fn close(&mut self) {
        self.inner.close();
    }
}

/// `client.analytics()`.
pub struct AnalyticsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> AnalyticsNamespace<'a> {
    fn slug(&self) -> Result<&'a str, KError> {
        require_project(self.config, "analytics")
    }

    fn dataset_path(&self, dataset: &str, suffix: &str) -> Result<String, KError> {
        let d = seg("dataset slug", dataset)?;
        Ok(project_path(
            self.slug()?,
            &format!("analytics/{d}{suffix}"),
        ))
    }

    /// `GET /@<slug>/analytics`: the project's active datasets, newest first.
    pub async fn list(&self) -> Result<Response<Listed<Dataset>>, KError> {
        let path = project_path(self.slug()?, "analytics");
        request::<Listed<Dataset>, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `POST /@<slug>/analytics/query`: one read-only SQL `SELECT` over datasets. Reference each as
    /// `dataset.<slug>` and list it in `datasets` (1 to 64). The rows are in `data`.
    pub async fn query(&self, sql: &str, datasets: &[&str]) -> Result<Response<Value>, KError> {
        let slug = self.slug()?;
        if sql.trim().is_empty() || sql.chars().count() > 100_000 {
            return Err(invalid(
                "analytics.query: sql must be 1 to 100000 characters",
            ));
        }
        if datasets.is_empty() || datasets.len() > 64 {
            return Err(invalid("analytics.query: datasets must list 1 to 64 slugs"));
        }
        let body = json!({ "sql": sql, "datasets": datasets });
        request::<Value, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "analytics/query"),
            Some(&body),
            None,
        )
        .await
    }

    /// `POST /@<slug>/analytics/<dataset>`: create a dataset at that slug. A slug is never reissued,
    /// even after a delete. A project holds at most 200 datasets (429 `K9O_DATASET_LIMIT_EXCEEDED`).
    pub async fn create(
        &self,
        dataset_slug: &str,
        input: &CreateDatasetInput,
    ) -> Result<Response<Dataset>, KError> {
        let path = self.dataset_path(dataset_slug, "")?;
        let n = input.display_name.chars().count();
        if !(1..=200).contains(&n) {
            return Err(invalid(
                "analytics.create: display_name must be 1 to 200 characters",
            ));
        }
        if input.columns.is_empty() || input.columns.len() > 512 {
            return Err(invalid(
                "analytics.create: columns must hold 1 to 512 entries",
            ));
        }
        if input.partition_by.as_ref().map_or(false, |p| p.len() > 16) {
            return Err(invalid(
                "analytics.create: partition_by holds at most 16 keys",
            ));
        }
        request::<Dataset, _>(self.config, Method::POST, &path, Some(input), None).await
    }

    /// `GET /@<slug>/analytics/<dataset>`: one active dataset (404 `K9O_DATASET_NOT_FOUND` when
    /// deleted or unknown).
    pub async fn get(&self, dataset_slug: &str) -> Result<Response<Dataset>, KError> {
        let path = self.dataset_path(dataset_slug, "")?;
        request::<Dataset, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `DELETE /@<slug>/analytics/<dataset>`: retire the dataset. Its slug is never reissued.
    pub async fn delete(&self, dataset_slug: &str) -> Result<Response<DatasetRetired>, KError> {
        let path = self.dataset_path(dataset_slug, "")?;
        request::<DatasetRetired, ()>(self.config, Method::DELETE, &path, None, None).await
    }

    /// `POST .../materialize`: copy rows into the dataset from exactly one of `from_collection` and
    /// `from_query` (a saved Studio query id that reads one collection).
    pub async fn materialize(
        &self,
        dataset_slug: &str,
        from_collection: Option<&str>,
        from_query: Option<&str>,
    ) -> Result<Response<MaterializeResult>, KError> {
        let path = self.dataset_path(dataset_slug, "/materialize")?;
        let body = match (from_collection, from_query) {
            (Some(c), None) if !c.is_empty() => json!({ "fromCollection": c }),
            (None, Some(q)) if !q.is_empty() => json!({ "fromQuery": q }),
            _ => {
                return Err(invalid(
                    "analytics.materialize: pass exactly one of from_collection and from_query",
                ))
            }
        };
        request::<MaterializeResult, Value>(self.config, Method::POST, &path, Some(&body), None)
            .await
    }

    /// `POST .../read`: one page of rows. Each row has a stable `_k9o_row_id`; `meta.cursor.next`
    /// continues, and the first page carries `meta.schema`.
    pub async fn read(
        &self,
        dataset_slug: &str,
        options: ReadOptions,
    ) -> Result<Response<Vec<Value>>, KError> {
        let path = self.dataset_path(dataset_slug, "/read")?;
        let mut body = json!({});
        if let Some(f) = options.partition_filter {
            body["partitionFilter"] = f;
        }
        if let Some(c) = options.cursor {
            body["cursor"] = json!(c);
        }
        if let Some(l) = options.limit {
            body["limit"] = json!(l);
        }
        request::<Vec<Value>, Value>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// `POST .../write`: append rows (newline-delimited JSON built from `rows`). Each row is checked
    /// against the dataset's schema; bad rows are skipped and reported unless `on_error` is `Reject`.
    pub async fn write(
        &self,
        dataset_slug: &str,
        rows: &[Value],
        options: WriteOptions,
    ) -> Result<Response<IngestResult>, KError> {
        let path = self.dataset_path(dataset_slug, "/write")?;
        if rows.is_empty() {
            return Err(invalid("analytics.write: rows must not be empty"));
        }
        let mut nd = String::new();
        for r in rows {
            nd.push_str(&serde_json::to_string(r).map_err(|e| invalid(e.to_string()))?);
            nd.push('\n');
        }
        let mut path = path;
        if let Some(e) = options.on_error {
            path.push_str(&format!("?onError={}", e.as_str()));
        }
        request_raw::<IngestResult>(
            self.config,
            Method::POST,
            &path,
            RawBody {
                content_type: "application/x-ndjson".into(),
                bytes: nd.into_bytes(),
            },
        )
        .await
    }

    /// `POST .../upload`: a CSV or JSON file of up to 25 MiB. Without a `manifest` the bytes are the
    /// raw body; with one the upload is multipart (`file` and `manifest`). `dry_run` returns the
    /// detected columns and a 50-row preview without writing. The original file is kept for
    /// [`Self::download_original`].
    pub async fn upload(
        &self,
        dataset_slug: &str,
        data: impl AsRef<[u8]>,
        options: UploadOptions,
    ) -> Result<Response<IngestResult>, KError> {
        let mut path = self.dataset_path(dataset_slug, "/upload")?;
        let data = data.as_ref();
        if data.is_empty() || data.len() > MAX_UPLOAD_BYTES {
            return Err(invalid(
                "analytics.upload: the file must be 1 byte to 25 MiB",
            ));
        }
        let mut q: Vec<(&str, String)> = vec![("format", options.format.as_str().to_string())];
        if options.dry_run {
            q.push(("dryRun", "true".into()));
        }
        if let Some(e) = options.on_error {
            q.push(("onError", e.as_str().into()));
        }
        if let Some(d) = &options.delimiter {
            q.push(("delimiter", d.clone()));
        }
        if let Some(e) = &options.encoding {
            q.push(("encoding", e.clone()));
        }
        if let Some(h) = options.has_header {
            q.push(("hasHeader", h.to_string()));
        }
        let qs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
        path.push('?');
        path.push_str(&qs.join("&"));
        let content_type = match options.format {
            UploadFormat::Csv => "text/csv",
            UploadFormat::Json => "application/json",
        };
        let raw = match &options.manifest {
            None => RawBody {
                content_type: content_type.into(),
                bytes: data.to_vec(),
            },
            Some(m) => {
                let b = boundary();
                let name = options
                    .file_name
                    .clone()
                    .unwrap_or_else(|| format!("upload.{}", options.format.as_str()));
                let fields = [("manifest", m.to_string())];
                RawBody {
                    content_type: format!("multipart/form-data; boundary={b}"),
                    bytes: multipart_body(&b, (&name, content_type, data), &fields),
                }
            }
        };
        request_raw::<IngestResult>(self.config, Method::POST, &path, raw).await
    }

    /// `GET .../upload/original`: the original file of the stored upload, as bytes (404
    /// `K9O_NOT_FOUND` when none is stored).
    pub async fn download_original(&self, dataset_slug: &str) -> Result<OriginalFile, KError> {
        let path = self.dataset_path(dataset_slug, "/upload/original")?;
        let d = request_download(self.config, &path).await?;
        Ok(OriginalFile {
            content_type: d.content_type,
            bytes: d.bytes,
        })
    }

    /// Continuous rollups of one dataset: `analytics().rollups().create(..)`.
    pub fn rollups(&self) -> DatasetRollupsNamespace<'a> {
        DatasetRollupsNamespace {
            config: self.config,
        }
    }

    /// Recurring materializations: `analytics().schedules().create(..)`.
    pub fn schedules(&self) -> DatasetSchedulesNamespace<'a> {
        DatasetSchedulesNamespace {
            config: self.config,
        }
    }
}

/// `client.analytics().rollups()`.
pub struct DatasetRollupsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl DatasetRollupsNamespace<'_> {
    fn path(&self, dataset: &str, suffix: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "analytics")?;
        let d = seg("dataset slug", dataset)?;
        Ok(project_path(
            slug,
            &format!("analytics/{d}/rollups{suffix}"),
        ))
    }

    /// `POST .../rollups`: a continuous rollup that folds a stream into the dataset over fixed
    /// windows. Without [`Self::reattest`] (a person only) it pauses after
    /// `reattest_interval_days`.
    pub async fn create(
        &self,
        dataset_slug: &str,
        input: &CreateRollupInput,
    ) -> Result<Response<Rollup>, KError> {
        let path = self.path(dataset_slug, "")?;
        if input.slug.is_empty() || input.slug.chars().count() > 63 {
            return Err(invalid("rollups.create: slug must be 1 to 63 characters"));
        }
        if input.source.is_empty() {
            return Err(invalid("rollups.create: source is required"));
        }
        if input.measures.is_empty() || input.measures.len() > 64 {
            return Err(invalid(
                "rollups.create: measures must hold 1 to 64 entries",
            ));
        }
        if input.dimensions.len() > 16 {
            return Err(invalid(
                "rollups.create: dimensions holds at most 16 entries",
            ));
        }
        if input.window_seconds < 60 {
            return Err(invalid(
                "rollups.create: window_seconds must be at least 60",
            ));
        }
        let personal = input
            .subject_field
            .as_deref()
            .map_or(false, |s| !s.is_empty());
        let non_personal = input.non_personal == Some(true);
        if personal == non_personal {
            return Err(invalid(
                "rollups.create: pass exactly one of subject_field and non_personal: true",
            ));
        }
        check_reattest(input.reattest_interval_days)?;
        request::<Rollup, _>(self.config, Method::POST, &path, Some(input), None).await
    }

    /// `GET .../rollups`: the dataset's rollups with their attestation state.
    pub async fn list(&self, dataset_slug: &str) -> Result<Response<Listed<Rollup>>, KError> {
        let path = self.path(dataset_slug, "")?;
        request::<Listed<Rollup>, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `DELETE .../rollups/<id>`.
    pub async fn delete(
        &self,
        dataset_slug: &str,
        rollup_id: &str,
    ) -> Result<Response<DeletedRef>, KError> {
        let id = seg("rollup id", rollup_id)?;
        let path = self.path(dataset_slug, &format!("/{id}"))?;
        request::<DeletedRef, ()>(self.config, Method::DELETE, &path, None, None).await
    }

    /// `POST .../rollups/<id>/reattest`: confirm the rollup is still wanted and optionally set a new
    /// interval (7, 30, 90 or 365 days). Only a person can do this: a service, workload or extension
    /// token gets 403 `K9O_RBAC_DENY` with `reason: human_only`; a personal access token acts as its
    /// person.
    pub async fn reattest(
        &self,
        dataset_slug: &str,
        rollup_id: &str,
        reattest_interval_days: Option<u16>,
    ) -> Result<Response<Rollup>, KError> {
        check_reattest(reattest_interval_days)?;
        let id = seg("rollup id", rollup_id)?;
        let path = self.path(dataset_slug, &format!("/{id}/reattest"))?;
        let body = reattest_body(reattest_interval_days);
        request::<Rollup, Value>(self.config, Method::POST, &path, Some(&body), None).await
    }
}

fn reattest_body(days: Option<u16>) -> Value {
    match days {
        Some(d) => json!({ "reattestIntervalDays": d }),
        None => json!({}),
    }
}

/// `client.analytics().schedules()`.
pub struct DatasetSchedulesNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl DatasetSchedulesNamespace<'_> {
    fn path(&self, dataset: &str, suffix: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "analytics")?;
        let d = seg("dataset slug", dataset)?;
        Ok(project_path(
            slug,
            &format!("analytics/{d}/schedules{suffix}"),
        ))
    }

    /// `POST .../schedules`: a recurring materialization from a collection. Each run reads the
    /// collection with its creator's current access. Without [`Self::reattest`] (a person only) it
    /// pauses after `reattest_interval_days`.
    pub async fn create(
        &self,
        dataset_slug: &str,
        input: &CreateDatasetScheduleInput,
    ) -> Result<Response<DatasetSchedule>, KError> {
        let path = self.path(dataset_slug, "")?;
        if input.from_collection.is_empty() {
            return Err(invalid("schedules.create: from_collection is required"));
        }
        if input.interval_seconds < 60 {
            return Err(invalid(
                "schedules.create: interval_seconds must be at least 60",
            ));
        }
        check_reattest(input.reattest_interval_days)?;
        request::<DatasetSchedule, _>(self.config, Method::POST, &path, Some(input), None).await
    }

    /// `GET .../schedules`: the dataset's schedules with their last and next run and attestation state.
    pub async fn list(
        &self,
        dataset_slug: &str,
    ) -> Result<Response<Listed<DatasetSchedule>>, KError> {
        let path = self.path(dataset_slug, "")?;
        request::<Listed<DatasetSchedule>, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `DELETE .../schedules/<id>`.
    pub async fn delete(
        &self,
        dataset_slug: &str,
        schedule_id: &str,
    ) -> Result<Response<DeletedRef>, KError> {
        let id = seg("schedule id", schedule_id)?;
        let path = self.path(dataset_slug, &format!("/{id}"))?;
        request::<DeletedRef, ()>(self.config, Method::DELETE, &path, None, None).await
    }

    /// `POST .../schedules/<id>/reattest`: confirm the schedule is still wanted (a paused schedule
    /// resumes) and optionally set a new interval (7, 30, 90 or 365 days). Only a person can do this:
    /// a service, workload or extension token gets 403 `K9O_RBAC_DENY` with `reason: human_only`; a
    /// personal access token acts as its person.
    pub async fn reattest(
        &self,
        dataset_slug: &str,
        schedule_id: &str,
        reattest_interval_days: Option<u16>,
    ) -> Result<Response<DatasetSchedule>, KError> {
        check_reattest(reattest_interval_days)?;
        let id = seg("schedule id", schedule_id)?;
        let path = self.path(dataset_slug, &format!("/{id}/reattest"))?;
        let body = reattest_body(reattest_interval_days);
        request::<DatasetSchedule, Value>(self.config, Method::POST, &path, Some(&body), None).await
    }
}

/// `client.rollups()`.
pub struct RollupsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl RollupsNamespace<'_> {
    fn rollup_path(&self, rollup_slug: &str, suffix: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "rollups")?;
        let r = seg("rollup slug", rollup_slug)?;
        Ok(project_path(slug, &format!("rollups/{r}{suffix}")))
    }

    /// `GET /@<slug>/rollups`: every rollup in the project whose dataset you can read.
    pub async fn list(&self) -> Result<Response<Listed<Rollup>>, KError> {
        let slug = require_project(self.config, "rollups")?;
        request::<Listed<Rollup>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "rollups"),
            None,
            None,
        )
        .await
    }

    /// `GET .../rollups/<slug>/read`: the rollup's cells for a time range. A cell below the cohort
    /// threshold reads `{ "status": "suppressed", "reason": "cohort_below_k" }`; `coverage` reports the
    /// range, the last fold and whether it is complete. The cells are in `data`.
    pub async fn read(
        &self,
        rollup_slug: &str,
        options: RollupReadOptions,
    ) -> Result<Response<Value>, KError> {
        let path = self.rollup_path(rollup_slug, "/read")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(v) = options.from {
            q.push(("from".into(), v));
        }
        if let Some(v) = options.to {
            q.push(("to".into(), v));
        }
        if let Some(d) = options.dims {
            q.push(("dims".into(), d.join(",")));
        }
        request::<Value, ()>(self.config, Method::GET, &path, None, Some(&q)).await
    }

    /// `GET .../rollups/<slug>/subscribe` (server-sent events): one `cell` event per closed cell.
    /// It is the change feed of `realtime().subscribe`: it reconnects with `Last-Event-ID`, drops
    /// repeats (by the frame's `eventId`) and calls `on_resync` when the position is lost; the action
    /// is `reread-source`: re-read the rollup with [`Self::read`] there. Each item is a
    /// [`RollupCell`]; a suppressed cell has `status` `suppressed` and a `reason`. The first
    /// connection is opened here. A connection ends after at most 300 seconds; the stream reconnects.
    pub async fn subscribe(
        &self,
        rollup_slug: &str,
        options: RollupSubscribeOptions,
    ) -> Result<RollupSubscription, KError> {
        let path = self.rollup_path(rollup_slug, "/subscribe")?;
        let initial = options
            .from_sequence
            .map(|n| vec![("fromSequence".to_string(), n.to_string())])
            .unwrap_or_default();
        let inner = open_feed_kind(
            self.config,
            path,
            Vec::new(),
            initial,
            options.feed,
            FeedKind::Rollup,
            None,
        )
        .await?;
        Ok(RollupSubscription { inner })
    }
}
