//! Vector collections — `client.vectors()`. A vector collection stores embeddings you computed
//! yourself, next to the text and metadata they came from, and answers nearest-neighbour searches.
//!
//! Wire mapping (project plane):
//!   - `nearest(collection, query_vector, opts)` -> `POST /@<slug>/vectors/<collection>/search`
//!   - `upsert(collection, rows)`                -> `POST /@<slug>/vectors/<collection>/upsert`
//!
//! Both send the vector in the request body, so they take real embeddings of any common size (up
//! to 16000 numbers). The collection name must start with `_`. The first upsert creates the
//! collection and fixes its dimension; every later vector must have the same length. Searching a
//! collection that was never upserted answers 404 `K9O_COLLECTION_NOT_FOUND`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_VECTOR: usize = 16000;
const MAX_ROWS: usize = 1000;
const METRICS: [&str; 3] = ["cosine", "euclidean", "dot"];

/// Where a vector came from: a source handle and the index of the chunk within it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorSource<'a> {
    /// 1 to 512 characters.
    pub handle: &'a str,
    pub chunk_index: u32,
}

/// Options of `vectors().nearest`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NearestOptions<'a> {
    /// How many results to return, 1 to 1000. The gateway defaults to 10.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Keep only results whose `score` is at least this. Default: no threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    /// `cosine` (default), `euclidean` or `dot`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<&'a str>,
    /// Equality filters on `id`, `contentHash`, `source.<key>` and `metadata.<key>`; every one
    /// must match. Values are strings, numbers or booleans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<&'a Map<String, Value>>,
}

/// One result of `vectors().nearest`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NearestVector {
    pub id: String,
    /// Higher is more similar. `cosine`: 1 minus the distance; `euclidean`: 1 / (1 + distance);
    /// `dot`: the inner product.
    pub score: f64,
    /// `{ handle, chunkIndex, .. }` as stored.
    #[serde(rename = "_source", default)]
    pub source: Value,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// From `metadata.model`, when set.
    #[serde(default)]
    pub model: Option<String>,
    /// From the source's `chunkIndex`, when set.
    #[serde(default)]
    pub chunk: Option<i64>,
}

/// Data of `vectors().nearest`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NearestResult {
    /// Closest first.
    pub results: Vec<NearestVector>,
    pub collection: String,
}

/// One row of `vectors().upsert`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorRow<'a> {
    /// 1 to 256 characters. Default: generated. Upserting an existing id overwrites that row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    /// 1 to 16000 numbers; every row in one request has the same length.
    pub vector: &'a [f64],
    #[serde(rename = "_source")]
    pub source: VectorSource<'a>,
    /// At most 128 characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<&'a str>,
    /// At most 100000 characters. Stored sealed when the row belongs to a subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a str>,
    /// Default `{}`. Keep only values here that you filter on; they are stored in plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Map<String, Value>>,
    /// The one end user the row belongs to (`["eu_..."]`). Required on a per-subject collection
    /// (the default for a new collection); refused on a collection declared shared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_ids: Option<&'a [&'a str]>,
}

/// Data of `vectors().upsert`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpsertResult {
    pub upserted: u32,
    pub collection: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NearestBody<'a> {
    query_vector: &'a [f64],
    #[serde(flatten)]
    opts: NearestOptions<'a>,
}

#[derive(Serialize)]
struct UpsertBody<'a> {
    rows: &'a [VectorRow<'a>],
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn check_vector(vector: &[f64], name: &str) -> Result<(), KError> {
    if vector.is_empty() || vector.len() > MAX_VECTOR {
        return Err(invalid(format!(
            "{name} must be an array of 1 to {MAX_VECTOR} numbers"
        )));
    }
    if !vector.iter().all(|n| n.is_finite()) {
        return Err(invalid(format!("{name} must contain only finite numbers")));
    }
    Ok(())
}

/// Vector collections namespace — `client.vectors()`.
pub struct VectorsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> VectorsNamespace<'a> {
    fn path(&self, collection: &str, tail: &str) -> Result<String, KError> {
        if !collection.starts_with('_') {
            return Err(invalid("a vector collection name must start with _")
                .with_context("collection", collection));
        }
        let slug = require_project(self.config, "vectors")?;
        Ok(project_path(
            slug,
            &format!("vectors/{}/{tail}", enc(collection)),
        ))
    }

    /// The rows nearest to `query_vector`, closest first. Answers 404
    /// `K9O_COLLECTION_NOT_FOUND` when the collection has never been upserted, and 400
    /// `K9O_INVALID_VECTOR_DIMENSION` when the vector has another length than the stored ones.
    pub async fn nearest(
        &self,
        collection: &str,
        query_vector: &[f64],
        opts: NearestOptions<'_>,
    ) -> Result<Response<NearestResult>, KError> {
        let path = self.path(collection, "search")?;
        check_vector(query_vector, "query_vector")?;
        if let Some(k) = opts.top_k {
            if !(1..=1000).contains(&k) {
                return Err(
                    invalid("top_k must be a whole number from 1 to 1000").with_context("top_k", k)
                );
            }
        }
        if let Some(s) = opts.min_score {
            if !s.is_finite() {
                return Err(invalid("min_score must be a number"));
            }
        }
        if let Some(m) = opts.metric {
            if !METRICS.contains(&m) {
                return Err(
                    invalid(format!("metric must be one of: {}", METRICS.join(", ")))
                        .with_context("metric", m),
                );
            }
        }
        if let Some(f) = opts.filters {
            for (k, v) in f {
                if !(v.is_string() || v.is_number() || v.is_boolean()) {
                    return Err(
                        invalid("filter values must be strings, numbers or booleans")
                            .with_context("filter", k.as_str()),
                    );
                }
            }
        }
        let body = NearestBody { query_vector, opts };
        request::<NearestResult, _>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// Insert or overwrite 1 to 1000 rows in one request. The first upsert creates the
    /// collection and fixes its dimension.
    pub async fn upsert(
        &self,
        collection: &str,
        rows: &[VectorRow<'_>],
    ) -> Result<Response<UpsertResult>, KError> {
        let path = self.path(collection, "upsert")?;
        if rows.is_empty() || rows.len() > MAX_ROWS {
            return Err(invalid(format!(
                "rows must be an array of 1 to {MAX_ROWS} rows"
            )));
        }
        for (i, row) in rows.iter().enumerate() {
            check_vector(row.vector, &format!("rows[{i}].vector"))?;
        }
        let body = UpsertBody { rows };
        request::<UpsertResult, _>(self.config, Method::POST, &path, Some(&body), None).await
    }
}
