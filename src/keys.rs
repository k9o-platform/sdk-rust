//! Key inventory: `client.keys()`.
//!
//! Wire mapping (under `/@<slug>/vault/keys`):
//!   - `keys().list(status)`   -> `GET  /vault/keys[?status=all]`
//!   - `keys().get(key_id)`    -> `GET  /vault/keys/<key_id>`
//!   - `keys().rotate(key_id, options)` -> `POST /vault/keys/<key_id>/rotate`
//!
//! Metadata only: no key material, and not your external key's identifier, is ever returned. Reading
//! needs PROJECT_ADMIN or PROJECT_AUDITOR; rotating needs PROJECT_ADMIN and a fresh step-up proof. Only a
//! person's credential works: end-user, extension, service and workload tokens are refused (403).

use crate::envelope::Response;
use crate::http::{request, request_with_headers, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The external key of a bring-your-own-key (BYOK) key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByokRef {
    /// The key management service, for example `aws-kms`.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// The first 16 characters of base64url(SHA-256) of your key's identifier, to match it to your key.
    #[serde(default)]
    pub external_key_fingerprint: Option<String>,
}

/// The external key of a hold-your-own-key (HYOK) key, held in your HSM cluster.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HyokRef {
    #[serde(default)]
    pub cluster_id: Option<String>,
    #[serde(default)]
    pub cluster_name: Option<String>,
    #[serde(default)]
    pub external_key_fingerprint: Option<String>,
}

/// One key version.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectKey {
    /// `vkey_<uuid>`.
    pub id: String,
    /// Starts at 1 and goes up by one on each rotation.
    #[serde(default)]
    pub version: Option<i64>,
    /// `active`, `rotating` or `revoked`.
    #[serde(default)]
    pub status: Option<String>,
    /// `PLATFORM_MANAGED`, `SKR`, `BYOK` or `HYOK`.
    #[serde(default)]
    pub custody_mode: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// When the key was last rotated; `None` if never.
    #[serde(default)]
    pub rotated_at: Option<String>,
    /// Set when the key is your own (BYOK).
    #[serde(default)]
    pub byok_ref: Option<ByokRef>,
    /// Set when the key is held in your HSM cluster (HYOK).
    #[serde(default)]
    pub hyok_ref: Option<HyokRef>,
}

/// The answer of `keys().list`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyList {
    /// The list is not paged: every matching key version is here.
    pub items: Vec<ProjectKey>,
}

/// Which key versions `keys().list` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatusFilter {
    /// Only the active key (the default).
    Active,
    /// The active key and the rotating and revoked versions.
    All,
}

/// The answer of `keys().rotate`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRotation {
    pub key_id: String,
    #[serde(default)]
    pub previous_version: Option<i64>,
    #[serde(default)]
    pub new_version: Option<i64>,
    /// `rotated`.
    #[serde(default)]
    pub status: Option<String>,
}

/// The step-up challenge of a 401 `K9O_STEP_UP_REQUIRED` (the error's `context.stepUp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpChallenge {
    /// Echoed back in the proof.
    pub scope_set_root: String,
    /// Echoed back in the proof; the challenge is single use.
    pub nonce: String,
    /// The lowest proof tier accepted, for example `T3_KEY_POSSESSION`.
    pub min_tier: Option<String>,
    /// The proof methods the gateway offers: `webauthn`, `dpop` or `mtls`.
    pub methods: Vec<String>,
    /// How old a proof may be, in seconds.
    pub max_age_sec: Option<u64>,
    /// The relying-party id, for a `webauthn` proof.
    pub rp_id: Option<String>,
}

impl StepUpChallenge {
    /// Read the challenge from a `K9O_STEP_UP_REQUIRED` error. `None` for any other error or when
    /// `scopeSetRoot` or `nonce` is missing.
    pub fn from_error(e: &KError) -> Option<Self> {
        if e.code != "K9O_STEP_UP_REQUIRED" {
            return None;
        }
        let c = e.context.get("stepUp")?.as_object()?;
        let text = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            scope_set_root: text("scopeSetRoot")?,
            nonce: text("nonce")?,
            min_tier: text("minTier"),
            methods: c
                .get("methods")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            max_age_sec: c.get("maxAgeSec").and_then(Value::as_u64),
            rp_id: text("rpId"),
        })
    }
}

/// What the step-up hook receives: the challenge and the request being retried. A DPoP proof binds
/// to the real `http_method` and `url`.
#[derive(Debug, Clone)]
pub struct StepUpRequest {
    pub challenge: StepUpChallenge,
    /// The HTTP method of the call, for example `POST`.
    pub http_method: String,
    /// The full URL of the call.
    pub url: String,
}

/// What the step-up hook returns: one of the offered `method`s and that method's own `proof`.
/// No route mints it; you produce it with your passkey, DPoP key or client certificate.
#[derive(Clone)]
pub struct StepUpProof {
    pub method: String,
    pub proof: String,
}

impl std::fmt::Debug for StepUpProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepUpProof")
            .field("method", &self.method)
            .field("proof", &"<redacted>")
            .finish()
    }
}

/// Boxed future returned by a step-up hook.
pub type StepUpFuture = Pin<Box<dyn Future<Output = Result<StepUpProof, KError>> + Send>>;

type StepUpHook = Arc<dyn Fn(StepUpRequest) -> StepUpFuture + Send + Sync>;

/// Options of `keys().rotate`.
#[derive(Clone, Default)]
pub struct RotateOptions {
    hook: Option<StepUpHook>,
}

impl RotateOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer a step-up challenge. When the gateway refuses with 401 `K9O_STEP_UP_REQUIRED`, the hook
    /// is called once with the challenge and the request; the SDK then repeats the same request with
    /// the `x-k9o-step-up-proof` header and never retries again. If the hook fails, its error is
    /// returned and nothing is retried.
    pub fn step_up<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(StepUpRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StepUpProof, KError>> + Send + 'static,
    {
        self.hook = Some(Arc::new(move |r| Box::pin(hook(r))));
        self
    }
}

impl std::fmt::Debug for RotateOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotateOptions")
            .field("step_up", &self.hook.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

/// `client.keys()`.
pub struct KeysNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl KeysNamespace<'_> {
    fn key_path(&self, key_id: &str, suffix: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "keys")?;
        if key_id.is_empty() || key_id == "." || key_id == ".." {
            return Err(KError::new(
                "K9O_INVALID_FIELD_VALUE",
                "keys: key_id must be a non-empty key id (vkey_...)",
            ));
        }
        Ok(project_path(
            slug,
            &format!("vault/keys/{}{}", enc(key_id), suffix),
        ))
    }

    /// `GET /@<slug>/vault/keys`: the project's encryption keys. By default only the active key;
    /// `Some(KeyStatusFilter::All)` adds the rotating and revoked versions. Not paged.
    pub async fn list(&self, status: Option<KeyStatusFilter>) -> Result<Response<KeyList>, KError> {
        let slug = require_project(self.config, "keys")?;
        let q = [("status".to_string(), "all".to_string())];
        let query: Option<&[(String, String)]> = match status {
            Some(KeyStatusFilter::All) => Some(&q),
            _ => None,
        };
        request::<KeyList, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "vault/keys"),
            None,
            query,
        )
        .await
    }

    /// `GET /@<slug>/vault/keys/<key_id>`: one key by id (404 `K9O_NOT_FOUND`).
    pub async fn get(&self, key_id: &str) -> Result<Response<ProjectKey>, KError> {
        let path = self.key_path(key_id, "")?;
        request::<ProjectKey, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// `POST /@<slug>/vault/keys/<key_id>/rotate`: rotate the project's active key. Every data key is
    /// re-wrapped under a new version, which becomes active. Needs PROJECT_ADMIN and a fresh step-up
    /// proof. Without the hook of [`RotateOptions::step_up`] the answer is 401 `K9O_STEP_UP_REQUIRED`
    /// and the challenge is in the error's `context` (read it with [`StepUpChallenge::from_error`]).
    /// With the hook the SDK answers the challenge once and repeats the request; a second 401 is
    /// returned as it is. A proof the gateway cannot read is 401 `K9O_STEP_UP_PROOF_MALFORMED`.
    /// Other refusals: 409 `K9O_STATE_CONFLICT` (`reason` `key_not_active` or `rotation_in_flight`);
    /// 429 `K9O_RATE_LIMITED` (`reason` `rotation_rate_cap`, at most 3 rotations in a rolling 24
    /// hours, or `key_service_busy`) with the wait in `retry_after_sec`. Not retried otherwise.
    pub async fn rotate(
        &self,
        key_id: &str,
        options: RotateOptions,
    ) -> Result<Response<KeyRotation>, KError> {
        let path = self.key_path(key_id, "/rotate")?;
        let first = request::<KeyRotation, ()>(self.config, Method::POST, &path, None, None).await;
        let (Err(err), Some(hook)) = (&first, &options.hook) else {
            return first;
        };
        let Some(challenge) = StepUpChallenge::from_error(err) else {
            return first;
        };
        let origin =
            crate::region::resolve_origin(&self.config.base_url, &self.config.region).await;
        let proof = hook(StepUpRequest {
            challenge: challenge.clone(),
            http_method: "POST".into(),
            url: format!("{origin}{path}"),
        })
        .await?;
        if !challenge.methods.is_empty() && !challenge.methods.contains(&proof.method) {
            return Err(KError::new(
                "K9O_INVALID_FIELD_VALUE",
                format!(
                    "keys.rotate: the step-up method {:?} is not one the gateway offered ({})",
                    proof.method,
                    challenge.methods.join(", ")
                ),
            ));
        }
        let envelope = serde_json::json!({
            "scopeSetRoot": challenge.scope_set_root,
            "nonce": challenge.nonce,
            "method": proof.method,
            "proof": proof.proof,
        });
        let header = URL_SAFE_NO_PAD.encode(envelope.to_string());
        let headers = [("x-k9o-step-up-proof".to_string(), header)];
        // Exactly once: whatever this answers is final.
        request_with_headers::<KeyRotation, ()>(
            self.config,
            Method::POST,
            &path,
            None,
            None,
            Some(&headers),
        )
        .await
    }
}
