//! Function lifecycle namespace — `client.functions()`.
//!
//! Wire mapping:
//!   - `upload_artifact(bytes, kind)` -> `POST /@<slug>/functions/artifacts` (raw bytes)
//!   - `deploy(input)`                -> `POST /@<slug>/functions`
//!   - `list(opts)`                   -> `GET /@<slug>/functions`
//!   - `get(name)` / `delete(name)`   -> `GET` / `DELETE /@<slug>/functions/<name>`
//!   - `invoke(name, payload)`        -> `POST /@<slug>/functions/<name>/invoke`
//!   - `triggers().list(name)`        -> `GET /@<slug>/functions/<name>/triggers`
//!   - `triggers().create(name, ..)`  -> `POST /@<slug>/functions/<name>/triggers`
//!   - `triggers().update(name, id, ..)` -> `PATCH /@<slug>/functions/<name>/triggers/<id>`
//!   - `triggers().delete(name, id)`  -> `DELETE /@<slug>/functions/<name>/triggers/<id>` (204, no body)

use crate::envelope::Response;
use crate::http::{request, request_raw, HttpClientConfig, RawBody};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Function record returned by gateway endpoints.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionRecord {
    pub id: String,
    pub name: String,
    pub runtime: String,
    pub tier: String,
    #[serde(default)]
    pub entry_point: Option<String>,
    #[serde(default)]
    pub version: String,
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub description: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub deployed_at: Option<String>,
    #[serde(default)]
    pub retired_at: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
}

/// Deploy input. The function's bytes are uploaded first
/// ([`FunctionsNamespace::upload_artifact`] returns the `artifact_key`);
/// deploy references that artifact.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployFunctionInput<'a> {
    /// Function name (1 to 128 characters).
    pub name: &'a str,
    /// Runtime, e.g. `"bun"`, `"python"`, `"go"`, `"rust"`.
    pub runtime: &'a str,
    /// `"gateway"` (WebAssembly in the gateway) or `"sandbox"` (an OCI image archive).
    pub tier: &'a str,
    /// Content-addressed key of the uploaded artifact.
    pub artifact_key: &'a str,
    /// Version label for this deploy (1 to 64 characters).
    pub version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
}

/// Optional query filters of `list`.
#[derive(Debug, Default, Clone)]
pub struct ListFunctionsOptions<'a> {
    pub limit: Option<u32>,
    pub cursor: Option<&'a str>,
    /// `"deploying"`, `"active"`, `"error"` or `"retired"`.
    pub status: Option<&'a str>,
    pub source: Option<&'a str>,
    pub tier: Option<&'a str>,
}

/// Optional invoke controls; property names are the gateway's.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeOptions<'a> {
    /// Run asynchronously instead of waiting for the result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation: Option<InvokeDelegation<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confined_to: Option<&'a str>,
}

/// A delegation token for an invoke.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeDelegation<'a> {
    pub token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invoke_session_id: Option<&'a str>,
}

/// Result of `upload_artifact`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactUpload {
    pub artifact_key: String,
}

#[derive(Serialize)]
struct InvokeBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a Value>,
    #[serde(flatten)]
    options: &'a InvokeOptions<'a>,
}

/// An event trigger: runs the function when a record event happens on a collection.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionTrigger {
    /// UUID.
    pub id: String,
    #[serde(default)]
    pub function_name: String,
    #[serde(default)]
    pub collection: Option<String>,
    /// Any of `insert`, `update`, `delete`, `upsert`.
    #[serde(default)]
    pub events: Vec<String>,
    /// A predicate over the record, or `None` to fire on every matching event.
    #[serde(default)]
    pub filter: Option<Value>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub log_ok_invocations: bool,
    #[serde(default)]
    pub created_at: String,
}

/// Input of `triggers().create`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTriggerInput<'a> {
    /// An existing collection: `^[a-zA-Z_][a-zA-Z0-9_]*$`, 1 to 128 characters.
    pub collection: &'a str,
    /// 1 to 4 of `insert`, `update`, `delete`, `upsert`.
    pub events: &'a [&'a str],
    /// Fire only when the record matches; `None` fires on every event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<&'a Value>,
    /// Default `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Record successful invocations too (default `false`: only failures are logged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_ok_invocations: Option<bool>,
}

/// Input of `triggers().update`: set at least one field. The collection cannot be changed.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTriggerInput<'a> {
    /// Replaces the whole list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<&'a [&'a str]>,
    /// `Some(Some(predicate))` replaces the filter, `Some(None)` clears it,
    /// `None` leaves it unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Option<&'a Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_ok_invocations: Option<bool>,
}

const TRIGGER_EVENTS: [&str; 4] = ["insert", "update", "delete", "upsert"];

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn check_events(events: &[&str]) -> Result<(), KError> {
    if events.is_empty() || events.len() > 4 || events.iter().any(|e| !TRIGGER_EVENTS.contains(e)) {
        return Err(
            invalid("events must list 1 to 4 of insert, update, delete, upsert")
                .with_context("field", "events"),
        );
    }
    Ok(())
}

fn check_trigger_collection(collection: &str) -> Result<(), KError> {
    let mut chars = collection.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && collection.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(
            invalid("collection must match ^[a-zA-Z_][a-zA-Z0-9_]*$ and be at most 128 characters")
                .with_context("field", "collection"),
        )
    }
}

fn check_segment(value: &str, what: &str) -> Result<(), KError> {
    if value.is_empty() {
        Err(invalid(format!("{what} must be non-empty")))
    } else {
        Ok(())
    }
}

/// Event triggers of one function — `client.functions().triggers()`.
pub struct TriggersNamespace<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> TriggersNamespace<'a> {
    fn path(&self, function: &str, tail: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "functions")?;
        check_segment(function, "function name")?;
        Ok(project_path(
            slug,
            &format!("functions/{}/triggers{}", enc(function), tail),
        ))
    }

    /// Every trigger of the function, newest first. Not paginated.
    pub async fn list(&self, function: &str) -> Result<Response<Vec<FunctionTrigger>>, KError> {
        let path = self.path(function, "")?;
        request::<Vec<FunctionTrigger>, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// Create an event trigger. The SDK checks `events` and the collection name
    /// before sending. Answers 201 with the trigger.
    pub async fn create(
        &self,
        function: &str,
        input: CreateTriggerInput<'_>,
    ) -> Result<Response<FunctionTrigger>, KError> {
        let path = self.path(function, "")?;
        check_trigger_collection(input.collection)?;
        check_events(input.events)?;
        request::<FunctionTrigger, _>(self.config, Method::POST, &path, Some(&input), None).await
    }

    /// Change a trigger. Set at least one field of `patch`.
    pub async fn update(
        &self,
        function: &str,
        trigger_id: &str,
        patch: UpdateTriggerInput<'_>,
    ) -> Result<Response<FunctionTrigger>, KError> {
        check_segment(trigger_id, "trigger id")?;
        let path = self.path(function, &format!("/{}", enc(trigger_id)))?;
        if patch.events.is_none()
            && patch.filter.is_none()
            && patch.enabled.is_none()
            && patch.log_ok_invocations.is_none()
        {
            return Err(invalid(
                "patch needs at least one of: events, filter, enabled, log_ok_invocations",
            ));
        }
        if let Some(events) = patch.events {
            check_events(events)?;
        }
        request::<FunctionTrigger, _>(self.config, Method::PATCH, &path, Some(&patch), None).await
    }

    /// Delete a trigger. The gateway answers 204, so `data` is `()`; deleting the
    /// same trigger again is a 404 `K9O_TRIGGER_NOT_FOUND`.
    pub async fn delete(&self, function: &str, trigger_id: &str) -> Result<Response<()>, KError> {
        check_segment(trigger_id, "trigger id")?;
        let path = self.path(function, &format!("/{}", enc(trigger_id)))?;
        request::<(), ()>(self.config, Method::DELETE, &path, None, None).await
    }
}

/// Functions namespace — `client.functions()`.
pub struct FunctionsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> FunctionsNamespace<'a> {
    /// Event triggers of a function.
    pub fn triggers(&self) -> TriggersNamespace<'a> {
        TriggersNamespace {
            config: self.config,
        }
    }

    /// Upload a function artifact and get back its content-addressed
    /// `artifact_key`, to pass to `deploy`: `POST /@<slug>/functions/artifacts`
    /// with the raw bytes. `kind` is `"wasm"` (a WebAssembly module for the
    /// gateway tier, up to 16 MiB; the gateway tier is for the platform operator and a project's deploy to it is
    /// refused) or `"tar"` (an OCI image archive for the
    /// sandbox tier, up to 256 MiB). Uploading the same bytes again returns
    /// the same key.
    pub async fn upload_artifact(
        &self,
        bytes: Vec<u8>,
        kind: &str,
    ) -> Result<Response<ArtifactUpload>, KError> {
        let slug = require_project(self.config, "functions")?;
        let content_type = match kind {
            "wasm" => "application/wasm",
            "tar" => "application/x-tar",
            _ => {
                return Err(KError::new(
                    "K9O_INVALID_FIELD_VALUE",
                    "artifact kind must be \"wasm\" or \"tar\"",
                )
                .with_context("field", "kind"))
            }
        };
        request_raw::<ArtifactUpload>(
            self.config,
            Method::POST,
            &project_path(slug, "functions/artifacts"),
            RawBody {
                content_type: content_type.to_string(),
                bytes,
            },
        )
        .await
    }

    /// Deploy or redeploy a function from an uploaded artifact. The record
    /// comes back with status `deploying`; it is ready when it flips to `active`.
    pub async fn deploy(
        &self,
        input: DeployFunctionInput<'_>,
    ) -> Result<Response<FunctionRecord>, KError> {
        let slug = require_project(self.config, "functions")?;
        request::<FunctionRecord, _>(
            self.config,
            Method::POST,
            &project_path(slug, "functions"),
            Some(&input),
            None,
        )
        .await
    }

    /// List deployed functions (cursor pagination).
    pub async fn list(
        &self,
        opts: ListFunctionsOptions<'_>,
    ) -> Result<Response<Vec<FunctionRecord>>, KError> {
        let slug = require_project(self.config, "functions")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(l) = opts.limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        if let Some(s) = opts.status {
            q.push(("status".into(), s.into()));
        }
        if let Some(s) = opts.source {
            q.push(("source".into(), s.into()));
        }
        if let Some(t) = opts.tier {
            q.push(("tier".into(), t.into()));
        }
        request::<Vec<FunctionRecord>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "functions"),
            None,
            Some(&q),
        )
        .await
    }

    pub async fn get(&self, name: &str) -> Result<Response<FunctionRecord>, KError> {
        let slug = require_project(self.config, "functions")?;
        request::<FunctionRecord, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("functions/{}", enc(name))),
            None,
            None,
        )
        .await
    }

    /// Retire a function. The result carries `retired` and `cooldownUntil`; the
    /// name enters a 24h cooldown.
    pub async fn delete(&self, name: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "functions")?;
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("functions/{}", enc(name))),
            None,
            None,
        )
        .await
    }

    /// Invoke a function. `payload` is sent as the `payload` field of the
    /// request body and passed to the function runtime verbatim (omitted when
    /// `None`); `options` add the gateway's other invoke controls.
    pub async fn invoke(
        &self,
        name: &str,
        payload: Option<&Value>,
        options: Option<&InvokeOptions<'_>>,
    ) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "functions")?;
        let default = InvokeOptions::default();
        let body = InvokeBody {
            payload,
            options: options.unwrap_or(&default),
        };
        request::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("functions/{}/invoke", enc(name))),
            Some(&body),
            None,
        )
        .await
    }
}
