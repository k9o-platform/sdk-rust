//! Storage namespace — `client.storage()`: per-project logical buckets and
//! the objects in them, against `/@<slug>/storage/buckets...`.
//!
//! Flat bucket methods on the namespace; object operations through
//! `storage.from(bucket)`. Signed URLs require an explicit `expires_in`
//! (1..=604800 seconds, no default). There is no signed PUT and no copy: the
//! gateway has no such routes. Uploads are a hand-built
//! `multipart/form-data` body (no extra crates); the bytes are sent as given
//! and never appear in an error this module constructs.
//!
//! Bucket names and object paths are percent-encoded, never concatenated
//! raw; `..` and a leading `/` are not rewritten (the server decides).

use crate::envelope::Response;
use crate::http::{request, request_raw, HttpClientConfig, RawBody};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

/// JS `encodeURIComponent` unreserved set; everything else (incl. `/`) is escaped.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

const MAX_EXPIRES_IN: i64 = 604_800;

/// A bucket.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageBucket {
    pub name: String,
    pub public: bool,
    pub file_size_limit: Option<u64>,
    #[serde(default)]
    pub allowed_mime_types: Vec<String>,
    pub versioning: bool,
    pub delete_after_days: Option<u32>,
    pub created_at: String,
    pub updated_at: String,
    pub system: bool,
    pub byos_binding_id: Option<String>,
}

/// Options for `create_bucket`; unset fields are omitted from the body.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBucketOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mime_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versioning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byos_binding_id: Option<String>,
}

/// Patch for `update_bucket`. Outer `None` = leave unchanged; for the
/// nullable fields `Some(None)` sends JSON `null` (clears the limit / unlinks).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBucketOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_limit: Option<Option<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mime_types: Option<Option<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versioning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_after_days: Option<Option<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byos_binding_id: Option<Option<String>>,
}

/// An object listing entry; `path` is relative to the bucket.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageObject {
    pub path: String,
    pub size: u64,
    pub last_modified: Option<String>,
}

/// Options for `upload`.
#[derive(Debug, Clone, Default)]
pub struct UploadOptions {
    pub content_type: Option<String>,
    /// `false` (default) refuses to overwrite an existing path.
    pub upsert: bool,
    /// At most 10 keys / 2 KB in total (enforced by the gateway).
    pub metadata: Option<HashMap<String, String>>,
}

/// Options for `create_signed_url`. `expires_in` is required (seconds).
#[derive(Debug, Clone)]
pub struct SignedUrlOptions {
    pub expires_in: i64,
    pub download: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedUrl {
    pub signed_url: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemovedObject {
    pub bucket: String,
    pub path: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedBucket {
    pub name: String,
    pub deleted: bool,
    pub objects_deleted: u64,
}

fn invalid(message: &str, field: &str, reason: &str) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
        .with_context("field", field)
        .with_context("reason", reason)
}

fn bucket_path(slug: &str, bucket: &str, suffix: &str) -> Result<String, KError> {
    if bucket.is_empty() {
        return Err(invalid("bucket name must not be empty", "bucket", "empty"));
    }
    // `.` and `..` survive percent-encoding and are collapsed as dot segments
    // by URL layers, so the request would address a different route.
    if bucket == "." || bucket == ".." {
        return Err(invalid(
            "bucket name must not be a dot segment",
            "bucket",
            "dot_segment",
        ));
    }
    let enc = utf8_percent_encode(bucket, COMPONENT);
    Ok(project_path(
        slug,
        &format!("storage/buckets/{}{}", enc, suffix),
    ))
}

fn require_object_path(path: &str) -> Result<(), KError> {
    if path.is_empty() {
        return Err(invalid("object path must not be empty", "path", "empty"));
    }
    Ok(())
}

fn require_expires_in(v: i64) -> Result<(), KError> {
    if !(1..=MAX_EXPIRES_IN).contains(&v) {
        return Err(KError::new(
            "K9O_INVALID_FIELD_VALUE",
            format!(
                "expires_in is required and must be an integer from 1 to {} seconds",
                MAX_EXPIRES_IN
            ),
        )
        .with_context("field", "expiresIn")
        .with_context("reason", "out_of_range")
        .with_recommendation(
            "Pass an explicit expires_in; signed URLs have no default lifetime.",
        ));
    }
    Ok(())
}

/// Quote a multipart header parameter value (CR/LF/quote/backslash escaped).
fn quote_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("%22"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn boundary() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("----k9oFormBoundary{:032x}", nanos)
}

/// Assemble a multipart/form-data body. `file` is `(filename, content_type, bytes)`.
pub(crate) fn multipart_body(
    boundary: &str,
    file: (&str, &str, &[u8]),
    fields: &[(&str, String)],
) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(file.2.len() + 512);
    b.extend_from_slice(
        format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"; filename={}\r\nContent-Type: {}\r\n\r\n",
            boundary,
            quote_param(file.0),
            file.1
        )
        .as_bytes(),
    );
    b.extend_from_slice(file.2);
    b.extend_from_slice(b"\r\n");
    for (name, value) in fields {
        b.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                boundary, name
            )
            .as_bytes(),
        );
        b.extend_from_slice(value.as_bytes());
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());
    b
}

/// Storage namespace — `client.storage()`.
pub struct StorageNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> StorageNamespace<'a> {
    fn slug(&self) -> Result<&'a str, KError> {
        require_project(self.config, "storage buckets and objects")
    }

    /// `GET /@<slug>/storage/buckets`.
    pub async fn list_buckets(&self) -> Result<Response<Vec<StorageBucket>>, KError> {
        let path = project_path(self.slug()?, "storage/buckets");
        request::<Vec<StorageBucket>, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `GET .../buckets/<name>`.
    pub async fn get_bucket(&self, name: &str) -> Result<Response<StorageBucket>, KError> {
        let path = bucket_path(self.slug()?, name, "")?;
        request::<StorageBucket, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `POST .../buckets` with `{ name, ...opts }`.
    pub async fn create_bucket(
        &self,
        name: &str,
        opts: CreateBucketOptions,
    ) -> Result<Response<StorageBucket>, KError> {
        let path = project_path(self.slug()?, "storage/buckets");
        let mut body = serde_json::to_value(&opts).unwrap_or_else(|_| json!({}));
        if let Value::Object(m) = &mut body {
            m.insert("name".into(), Value::String(name.to_string()));
        }
        request::<StorageBucket, _>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// `PATCH .../buckets/<name>`.
    pub async fn update_bucket(
        &self,
        name: &str,
        patch: UpdateBucketOptions,
    ) -> Result<Response<StorageBucket>, KError> {
        let path = bucket_path(self.slug()?, name, "")?;
        request::<StorageBucket, _>(self.config, Method::PATCH, &path, Some(&patch), None).await
    }

    /// `DELETE .../buckets/<name>`; `?empty=true` only when `empty` is true.
    pub async fn delete_bucket(
        &self,
        name: &str,
        empty: bool,
    ) -> Result<Response<DeletedBucket>, KError> {
        let path = bucket_path(self.slug()?, name, "")?;
        let q = [("empty".to_string(), "true".to_string())];
        let query: Option<&[(String, String)]> = if empty { Some(&q) } else { None };
        request::<DeletedBucket, ()>(self.config, Method::DELETE, &path, None, query).await
    }

    /// A handle on one bucket for object operations.
    pub fn from(&self, bucket: &str) -> BucketHandle<'a> {
        BucketHandle {
            config: self.config,
            bucket: bucket.to_string(),
        }
    }
}

/// Object operations on one bucket: `client.storage().from("avatars")`.
pub struct BucketHandle<'a> {
    config: &'a HttpClientConfig,
    bucket: String,
}

impl<'a> BucketHandle<'a> {
    fn objects_path(&self, suffix: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "storage buckets and objects")?;
        bucket_path(slug, &self.bucket, &format!("/objects{}", suffix))
    }

    /// `POST .../objects` (multipart: `file`, `path`, optional `contentType`,
    /// `upsert`, `metadata`). Bytes are sent exactly as given.
    pub async fn upload(
        &self,
        path: &str,
        data: impl AsRef<[u8]>,
        opts: UploadOptions,
    ) -> Result<Response<StorageObject>, KError> {
        let url_path = self.objects_path("")?;
        require_object_path(path)?;
        let filename = path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("file");
        let file_ct = opts
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream");
        let mut fields: Vec<(&str, String)> = vec![("path", path.to_string())];
        if let Some(ct) = &opts.content_type {
            fields.push(("contentType", ct.clone()));
        }
        fields.push((
            "upsert",
            if opts.upsert { "true" } else { "false" }.to_string(),
        ));
        if let Some(m) = &opts.metadata {
            fields.push((
                "metadata",
                serde_json::to_string(m).unwrap_or_else(|_| "{}".into()),
            ));
        }
        let boundary = boundary();
        let bytes = multipart_body(&boundary, (filename, file_ct, data.as_ref()), &fields);
        request_raw::<StorageObject>(
            self.config,
            Method::POST,
            &url_path,
            RawBody {
                content_type: format!("multipart/form-data; boundary={}", boundary),
                bytes,
            },
        )
        .await
    }

    /// `POST .../objects/sign` with `{ path, expiresIn, download? }`.
    /// `expires_in` must be 1..=604800 or the call is refused before any request.
    pub async fn create_signed_url(
        &self,
        path: &str,
        opts: SignedUrlOptions,
    ) -> Result<Response<SignedUrl>, KError> {
        let url_path = self.objects_path("/sign")?;
        require_object_path(path)?;
        require_expires_in(opts.expires_in)?;
        let mut body = json!({ "path": path, "expiresIn": opts.expires_in });
        if let (Some(d), Value::Object(m)) = (opts.download, &mut body) {
            m.insert("download".into(), Value::Bool(d));
        }
        request::<SignedUrl, _>(self.config, Method::POST, &url_path, Some(&body), None).await
    }

    /// `GET .../objects`, with `?prefix=` only when given.
    pub async fn list(&self, prefix: Option<&str>) -> Result<Response<Vec<StorageObject>>, KError> {
        let url_path = self.objects_path("")?;
        let q = prefix.map(|p| [("prefix".to_string(), p.to_string())]);
        request::<Vec<StorageObject>, ()>(
            self.config,
            Method::GET,
            &url_path,
            None,
            q.as_ref().map(|a| &a[..]),
        )
        .await
    }

    /// `DELETE .../objects?path=`.
    pub async fn remove(&self, path: &str) -> Result<Response<RemovedObject>, KError> {
        let url_path = self.objects_path("")?;
        require_object_path(path)?;
        let q = [("path".to_string(), path.to_string())];
        request::<RemovedObject, ()>(self.config, Method::DELETE, &url_path, None, Some(&q)).await
    }
}
