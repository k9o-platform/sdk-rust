//! Metadata namespace: `client.metadata()`. Metadata is a set of columns you declare on a collection (or on
//! another data surface) so that they can be filtered, searched and followed live through one API, with the
//! same access rules as the data itself.
//!
//! Wire mapping (project plane):
//!   - `filter(MetadataFilterInput)`                 -> `POST /@<slug>/metadata/query`
//!   - `set(collection, id, &metadata)`              -> `PATCH /@<slug>/metadata/collection/<collection>/<id>`
//!   - `search(MetadataSearchInput)`                 -> `POST /@<slug>/metadata/search`
//!   - `vectors().nearest(MetadataNearestInput)`     -> `POST /@<slug>/metadata/vectors/nearest`
//!   - `subscribe(surface, options)`                 -> `GET  /@<slug>/metadata/subscribe?surface=` (SSE)
//!   - `index().get(collection)`                     -> `GET  /@<slug>/metadata/index/<collection>`
//!   - `index().declare(collection, &columns)`       -> `PUT  /@<slug>/metadata/index/<collection>`
//!
//! Columns are declared first (`index().declare`); `set` writes declared columns only, and `filter`, `search`
//! and `vectors().nearest` read declared columns only. Column names given to `filter` (`where` fields), `search`
//! (`field`) and `vectors().nearest` (`column`) are sent as `metadata.<name>`; the prefix is added when you leave
//! it out. Rows come back with bare column names under `metadata`.
//!
//! Every call checks its input before sending; a bad input fails with `K9O_INVALID_FIELD_VALUE` (or
//! `K9O_UNKNOWN_SURFACE` for a surface) and nothing is sent. Errors from the gateway arrive as [`KError`]
//! with the gateway's code. The ones to expect:
//!   - 400 `K9O_UNKNOWN_SURFACE`: the surface is not `collection:<name>`, `telemetry:<event>` or `storage:<bucket>`.
//!   - 400 `K9O_METADATA_FIELD_NOT_INDEXED`: the column is not declared (`context["field"]`).
//!   - 400 `K9O_ENCRYPTED_COLUMN_OPERATION_UNSUPPORTED`: an encrypted column supports only `eq` (not null) and `in`,
//!     no search or nearest.
//!   - 400 `K9O_INVALID_FIELD_VALUE`, `K9O_INVALID_CURSOR`: a bad body field or cursor.
//!   - 403 `K9O_ACCESS_DENIED`, `K9O_EXTENSION_PERMISSION_DENIED`: no access to the surface.
//!   - 404 `K9O_COLLECTION_NOT_FOUND` (declare on a missing collection), `K9O_NOT_FOUND` (set on a missing record).
//!   - 413 `K9O_QUERY_RESULT_TOO_LARGE`: more than 10000 rows across the surfaces (`context["cap"]`, `context["surfaces"]`).
//!   - 501 `K9O_NOT_IMPLEMENTED`: `storage:` surfaces pass the SDK's check and the gateway answers 501 for them.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::realtime::{open_feed, Resync, SubscribeOptions, Subscription};
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::future::Future;
use std::time::Duration;

const MAX_COLUMNS: usize = 64;
const MAX_LIMIT: u32 = 1000;
const PREFIX: &str = "metadata.";
/// On a telemetry surface these where fields are bare, never prefixed.
const TELEMETRY_FIELDS: [&str; 2] = ["createdAt", "actorId"];

/// Comparison operators of a `where` clause. `And`, `Or` and `Not` are part of the grammar, but the gateway
/// refuses them today (400 `K9O_INVALID_FIELD_VALUE` on a collection surface, `K9O_METADATA_FIELD_NOT_INDEXED` on
/// telemetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MetadataOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains,
    StartsWith,
    EndsWith,
    In,
    NotIn,
    And,
    Or,
    Not,
}

/// One condition. On a `collection:` surface `field` is a declared column (`priority` or `metadata.priority`; the
/// prefix is added when missing) and each field may appear once. On a `telemetry:` surface the fields are
/// `createdAt` (with `Gt`, `Gte`, `Lt`, `Lte`) and `actorId` (with `Eq`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetadataWhere {
    pub field: String,
    pub op: MetadataOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

impl MetadataWhere {
    pub fn new(field: impl Into<String>, op: MetadataOp, value: impl Into<Value>) -> Self {
        Self {
            field: field.into(),
            op,
            value: Some(value.into()),
        }
    }
}

/// One result row. A `collection:` row has `record_id`; a `telemetry:` row has `event_id`. `metadata` holds the
/// declared columns that are present on the row, under their bare names (an undeclared column never appears).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MetadataRow {
    #[serde(default)]
    pub surface: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub record_id: Option<String>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

/// Input of `metadata().filter`.
#[derive(Debug, Clone, Default)]
pub struct MetadataFilterInput {
    /// One or more surfaces: `collection:<name>`, `telemetry:<event-prefix>` or `storage:<bucket>`.
    pub surfaces: Vec<String>,
    /// Conditions applied to every surface named.
    pub r#where: Option<Vec<MetadataWhere>>,
    /// Rows per surface, 1 to 1000. The gateway defaults to 100.
    pub limit: Option<u32>,
    /// `meta.cursor.next` of the previous page. Allowed with exactly one surface.
    pub cursor: Option<String>,
}

/// Input of `metadata().search`.
#[derive(Debug, Clone, Default)]
pub struct MetadataSearchInput {
    /// A `collection:<name>` surface.
    pub surface: String,
    /// The declared text column: `body` or `metadata.body`.
    pub field: String,
    /// The words to look for. Matching is by word stem, so "finance" does not match "financial".
    pub q: String,
    /// 1 to 1000. The gateway defaults to 50. Search has no second page.
    pub limit: Option<u32>,
}

/// Input of `metadata().vectors().nearest`.
#[derive(Debug, Clone, Default)]
pub struct MetadataNearestInput {
    /// A `collection:<name>` surface.
    pub surface: String,
    /// The declared vector column: `embedding` or `metadata.embedding`.
    pub column: String,
    /// The query embedding: a non-empty list of finite numbers.
    pub embedding: Vec<f64>,
    /// How many rows to return, 1 to 1000. The gateway defaults to 10.
    pub k: Option<u32>,
    /// `cosine` (default), `euclidean` or `dot`.
    pub metric: Option<String>,
    /// Conditions that rows must also meet; the same grammar as `filter`.
    pub r#where: Option<Vec<MetadataWhere>>,
}

/// The payload of a metadata change event: which record changed. The new values are not in the event; read the
/// record by `id`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MetadataChange {
    #[serde(default)]
    pub collection: String,
    #[serde(default)]
    pub id: String,
}

impl MetadataChange {
    /// Read `{ collection, id }` from the `payload` of a [`crate::RealtimeEvent`] delivered by
    /// `metadata().subscribe`.
    pub fn from_payload(payload: &Value) -> Option<Self> {
        serde_json::from_value(payload.clone()).ok()
    }
}

/// Options of `metadata().subscribe`.
#[derive(Clone, Default)]
pub struct MetadataSubscribeOptions {
    /// Start from this feed position instead of from now. Sent on the first connection only; ignored once the
    /// subscription has resumed by itself or been resynced.
    pub from_sequence: Option<u64>,
    feed: SubscribeOptions,
}

impl MetadataSubscribeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_sequence(mut self, sequence: u64) -> Self {
        self.from_sequence = Some(sequence);
        self
    }

    /// Called when the server cannot resume from the last event. Re-read the records through `filter` inside it;
    /// the subscription then continues on a fresh connection.
    pub fn on_resync<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Resync) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.feed = self.feed.on_resync(handler);
        self
    }

    /// Pause before a reconnect. Default: the server's `retry:` hint, else one second.
    pub fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.feed = self.feed.reconnect_delay(delay);
        self
    }

    /// Reconnect when no bytes arrive for this long. Default 45 seconds.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.feed = self.feed.idle_timeout(timeout);
        self
    }
}

impl std::fmt::Debug for MetadataSubscribeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataSubscribeOptions")
            .field("from_sequence", &self.from_sequence)
            .field("feed", &self.feed)
            .finish()
    }
}

/// One column of a declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetadataIndexColumn {
    pub column: String,
    /// Default false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed: Option<bool>,
}

impl MetadataIndexColumn {
    pub fn new(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            indexed: None,
        }
    }

    pub fn indexed(mut self, indexed: bool) -> Self {
        self.indexed = Some(indexed);
        self
    }
}

/// A declared column as the gateway reports it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MetadataIndexEntry {
    #[serde(default)]
    pub column: String,
    #[serde(default)]
    pub indexed: bool,
}

/// A collection's declaration: its declared columns, ordered by name.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MetadataIndex {
    #[serde(default)]
    pub collection: String,
    #[serde(default)]
    pub columns: Vec<MetadataIndexEntry>,
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

/// `[A-Za-z0-9][A-Za-z0-9._:-]{0,127}`
fn valid_scope(scope: &str) -> bool {
    let mut chars = scope.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    scope.len() <= 128
        && first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// `[a-zA-Z_][a-zA-Z0-9_]{0,62}`
fn valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 63
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn slice_of(surface: &str) -> &str {
    surface.split_once(':').map(|(s, _)| s).unwrap_or("")
}

fn check_surface(surface: &str, name: &str) -> Result<(), KError> {
    let (slice, scope) = surface.split_once(':').unwrap_or(("", ""));
    let ok = matches!(slice, "collection" | "telemetry" | "storage")
        && valid_scope(scope)
        && (slice != "collection" || valid_ident(scope));
    if ok {
        return Ok(());
    }
    Err(KError::new(
        "K9O_UNKNOWN_SURFACE",
        format!(
            "{name} \"{surface}\" is not a surface: use collection:<name>, telemetry:<event> or storage:<bucket>"
        ),
    )
    .with_context("surface", surface))
}

fn check_collection_surface(surface: &str, name: &str) -> Result<(), KError> {
    check_surface(surface, name)?;
    if slice_of(surface) != "collection" {
        return Err(invalid(format!(
            "{name} must be a collection surface (collection:<name>)"
        ))
        .with_context("surface", surface));
    }
    Ok(())
}

fn check_limit(value: Option<u32>, name: &str) -> Result<(), KError> {
    match value {
        Some(v) if !(1..=MAX_LIMIT).contains(&v) => Err(invalid(format!(
            "{name} must be a whole number from 1 to {MAX_LIMIT}"
        ))
        .with_context(name, v)),
        _ => Ok(()),
    }
}

/// The name on the wire: `metadata.<name>`, adding the prefix when it is missing.
fn prefixed(name: &str, label: &str) -> Result<String, KError> {
    if name.is_empty() || name == PREFIX {
        return Err(invalid(format!("{label} must be a column name")).with_context(label, name));
    }
    Ok(if name.starts_with(PREFIX) {
        name.to_string()
    } else {
        format!("{PREFIX}{name}")
    })
}

fn wire_where(where_: &[MetadataWhere], telemetry: bool) -> Result<Vec<Value>, KError> {
    let mut out = Vec::with_capacity(where_.len());
    for (index, clause) in where_.iter().enumerate() {
        let bare = telemetry && TELEMETRY_FIELDS.contains(&clause.field.as_str());
        let field = if bare {
            clause.field.clone()
        } else {
            prefixed(&clause.field, &format!("where[{index}].field"))?
        };
        let wire = MetadataWhere {
            field,
            op: clause.op,
            value: clause.value.clone(),
        };
        out.push(serde_json::to_value(&wire).expect("where serialises"));
    }
    Ok(out)
}

fn project_slug(config: &HttpClientConfig) -> Result<&str, KError> {
    require_project(config, "metadata")
}

fn page_path(config: &HttpClientConfig, tail: &str) -> Result<String, KError> {
    Ok(project_path(
        project_slug(config)?,
        &format!("metadata/{tail}"),
    ))
}

fn check_collection(collection: &str) -> Result<(), KError> {
    if !valid_ident(collection) {
        return Err(
            invalid("collection must be a collection name").with_context("collection", collection)
        );
    }
    Ok(())
}

/// Metadata namespace — `client.metadata()`.
pub struct MetadataNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> MetadataNamespace<'a> {
    /// Rows of one or more surfaces whose declared columns meet `where`, in request order. Pages with `cursor`
    /// when exactly one surface is named (`meta.cursor.next`, `None` on the last page). More than 10000 rows in
    /// all answers 413 `K9O_QUERY_RESULT_TOO_LARGE`; a `storage:` surface answers 501 `K9O_NOT_IMPLEMENTED`.
    pub async fn filter(
        &self,
        input: MetadataFilterInput,
    ) -> Result<Response<Vec<MetadataRow>>, KError> {
        let path = page_path(self.config, "query")?;
        if input.surfaces.is_empty() {
            return Err(invalid("surfaces must be a non-empty list"));
        }
        for (i, s) in input.surfaces.iter().enumerate() {
            check_surface(s, &format!("surfaces[{i}]"))?;
        }
        check_limit(input.limit, "limit")?;
        if let Some(cursor) = &input.cursor {
            if cursor.is_empty() {
                return Err(invalid("cursor must be the cursor of a previous page"));
            }
            if input.surfaces.len() != 1 {
                return Err(invalid("cursor is allowed only with exactly one surface")
                    .with_context("field", "cursor"));
            }
        }
        let telemetry = input.surfaces.iter().any(|s| slice_of(s) == "telemetry");
        let mut body = Map::new();
        body.insert("surfaces".into(), json!(input.surfaces));
        if let Some(w) = &input.r#where {
            body.insert("where".into(), Value::Array(wire_where(w, telemetry)?));
        }
        if let Some(l) = input.limit {
            body.insert("limit".into(), json!(l));
        }
        if let Some(c) = &input.cursor {
            body.insert("cursor".into(), json!(c));
        }
        request::<Vec<MetadataRow>, _>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// Write declared columns of one record: `set("posts", "p2", &map)`. Keys are bare column names (a leading
    /// `metadata.` is dropped); a column that is not declared answers 400 `K9O_METADATA_FIELD_NOT_INDEXED`.
    /// Returns the whole updated record, including columns that are not declared. The write runs through the
    /// record API's rules, so a write that needs approval is queued as it is there.
    pub async fn set(
        &self,
        collection: &str,
        id: &str,
        metadata: &Map<String, Value>,
    ) -> Result<Response<Map<String, Value>>, KError> {
        let slug = project_slug(self.config)?;
        check_collection(collection)?;
        if id.is_empty() {
            return Err(invalid("id must be a record id").with_context("id", id));
        }
        if metadata.is_empty() {
            return Err(invalid(
                "metadata must be an object with at least one column",
            ));
        }
        let mut bare = Map::new();
        for (key, value) in metadata {
            let name = key.strip_prefix(PREFIX).unwrap_or(key);
            bare.insert(name.to_string(), value.clone());
        }
        let path = project_path(
            slug,
            &format!("metadata/collection/{}/{}", enc(collection), enc(id)),
        );
        let body = json!({ "metadata": bare });
        request::<Map<String, Value>, _>(self.config, Method::PATCH, &path, Some(&body), None).await
    }

    /// Full-text search over one declared text column of a collection. Returns the first page only (there is no
    /// cursor input), with no rank.
    pub async fn search(
        &self,
        input: MetadataSearchInput,
    ) -> Result<Response<Vec<MetadataRow>>, KError> {
        let path = page_path(self.config, "search")?;
        check_collection_surface(&input.surface, "surface")?;
        let field = prefixed(&input.field, "field")?;
        if input.q.is_empty() {
            return Err(invalid("q must be a non-empty string"));
        }
        check_limit(input.limit, "limit")?;
        let mut body = Map::new();
        body.insert("surface".into(), json!(input.surface));
        body.insert("field".into(), json!(field));
        body.insert("q".into(), json!(input.q));
        if let Some(l) = input.limit {
            body.insert("limit".into(), json!(l));
        }
        request::<Vec<MetadataRow>, _>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// Vector queries over declared vector columns.
    pub fn vectors(&self) -> MetadataVectorsNamespace<'a> {
        MetadataVectorsNamespace {
            config: self.config,
        }
    }

    /// The declaration of the columns metadata may use.
    pub fn index(&self) -> MetadataIndexNamespace<'a> {
        MetadataIndexNamespace {
            config: self.config,
        }
    }

    /// Follow changes of a `collection:<name>` surface live. Each event is `row_inserted`, `row_updated` or
    /// `row_deleted` with `payload` `{ collection, id }` only (see [`MetadataChange::from_payload`]), so read the
    /// record by id for its values. It is the change-feed reader of `realtime().subscribe`: it reconnects with
    /// `Last-Event-ID`, drops events it already delivered and calls `on_resync` when the position is lost.
    /// `from_sequence` starts from a known position. Changes to undeclared columns are reported too. Each event's
    /// `event_id` is the id of its frame. Realtime access is required on top of record read access.
    pub async fn subscribe(
        &self,
        surface: &str,
        options: MetadataSubscribeOptions,
    ) -> Result<Subscription, KError> {
        let slug = project_slug(self.config)?;
        check_collection_surface(surface, "surface")?;
        let initial = options
            .from_sequence
            .map(|n| vec![("fromSequence".to_string(), n.to_string())])
            .unwrap_or_default();
        open_feed(
            self.config,
            project_path(slug, "metadata/subscribe"),
            vec![("surface".into(), surface.into())],
            initial,
            options.feed,
        )
        .await
    }
}

/// `client.metadata().vectors()`.
pub struct MetadataVectorsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl MetadataVectorsNamespace<'_> {
    /// The rows whose declared vector column is nearest to `embedding`, closest first. Rows carry no score. At
    /// most 50 rows come back whatever `k` is, until the gateway forwards a larger page size. There is no next
    /// page: `meta.cursor.next` is `None`.
    pub async fn nearest(
        &self,
        input: MetadataNearestInput,
    ) -> Result<Response<Vec<MetadataRow>>, KError> {
        let path = page_path(self.config, "vectors/nearest")?;
        check_collection_surface(&input.surface, "surface")?;
        let column = prefixed(&input.column, "column")?;
        if input.embedding.is_empty() {
            return Err(invalid("embedding must be a non-empty list of numbers"));
        }
        if !input.embedding.iter().all(|n| n.is_finite()) {
            return Err(invalid("embedding must contain only finite numbers"));
        }
        check_limit(input.k, "k")?;
        if let Some(m) = &input.metric {
            if !["cosine", "euclidean", "dot"].contains(&m.as_str()) {
                return Err(invalid("metric must be one of: cosine, euclidean, dot")
                    .with_context("metric", m.as_str()));
            }
        }
        let mut body = Map::new();
        body.insert("surface".into(), json!(input.surface));
        body.insert("column".into(), json!(column));
        body.insert("embedding".into(), json!(input.embedding));
        if let Some(k) = input.k {
            body.insert("k".into(), json!(k));
        }
        if let Some(m) = &input.metric {
            body.insert("metric".into(), json!(m));
        }
        if let Some(w) = &input.r#where {
            body.insert("where".into(), Value::Array(wire_where(w, false)?));
        }
        request::<Vec<MetadataRow>, _>(self.config, Method::POST, &path, Some(&body), None).await
    }
}

/// `client.metadata().index()`.
pub struct MetadataIndexNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl MetadataIndexNamespace<'_> {
    /// The columns declared on a collection, ordered by name.
    pub async fn get(&self, collection: &str) -> Result<Response<MetadataIndex>, KError> {
        let slug = project_slug(self.config)?;
        check_collection(collection)?;
        let path = project_path(slug, &format!("metadata/index/{}", enc(collection)));
        request::<MetadataIndex, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// Declare the columns of a collection that metadata may use. The columns must already exist on the
    /// collection. The list replaces the previous declaration (an empty list clears it); at most 64 columns, no
    /// `id`, no duplicates. Needs the project-admin role and the migrations permission.
    pub async fn declare(
        &self,
        collection: &str,
        columns: &[MetadataIndexColumn],
    ) -> Result<Response<MetadataIndex>, KError> {
        let slug = project_slug(self.config)?;
        check_collection(collection)?;
        if columns.len() > MAX_COLUMNS {
            return Err(
                invalid(format!("at most {MAX_COLUMNS} columns can be declared"))
                    .with_context("count", columns.len()),
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (index, c) in columns.iter().enumerate() {
            if !valid_ident(&c.column) || c.column == "id" {
                return Err(invalid(format!(
                    "columns[{index}].column must be a column name other than id"
                ))
                .with_context("column", c.column.as_str()));
            }
            if !seen.insert(c.column.as_str()) {
                return Err(invalid(format!("column \"{}\" is listed twice", c.column))
                    .with_context("column", c.column.as_str()));
            }
        }
        let path = project_path(slug, &format!("metadata/index/{}", enc(collection)));
        let body = json!({ "columns": columns });
        request::<MetadataIndex, _>(self.config, Method::PUT, &path, Some(&body), None).await
    }
}
