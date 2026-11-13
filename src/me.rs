//! Operator-self namespace — `client.me()`.
//!
//! Cross-org; org-plane routes under `/me`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::url_grammar::{enc, org_path};
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Serialize)]
struct DisplayNameBody<'a> {
    #[serde(rename = "displayName")]
    display_name: &'a str,
}

/// Scope of a personal API token. Every dimension is optional; an unset
/// dimension is unconstrained. `projects` must name exactly one project.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiTokenScopeManifest<'a> {
    /// `"readonly"`, `"readwrite"` or `"admin"`; each includes the one before. The gateway requires it on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_classes: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collections: Option<HashMap<&'a str, Vec<&'a str>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cidrs: Option<Vec<&'a str>>,
}

/// Body of `me().api_tokens().create`; property names are the gateway's.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateApiTokenInput<'a> {
    /// Names the token (1 to 80 characters).
    pub label: &'a str,
    pub scope_manifest: ApiTokenScopeManifest<'a>,
    /// Token lifetime; omitted uses the gateway default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
}

/// Me namespace — `client.me()`. Accessors return sub-namespace handles.
pub struct MeNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> MeNamespace<'a> {
    pub fn profile(&self) -> MeProfile<'a> {
        MeProfile {
            config: self.config,
        }
    }
    pub fn sessions(&self) -> MeSessions<'a> {
        MeSessions {
            config: self.config,
        }
    }
    pub fn api_tokens(&self) -> MeApiTokens<'a> {
        MeApiTokens {
            config: self.config,
        }
    }
}

pub struct MeProfile<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> MeProfile<'a> {
    /// `GET /me`.
    pub async fn get(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(self.config, Method::GET, &org_path("me"), None, None).await
    }

    /// `PATCH /me` with `{displayName}`.
    pub async fn update(&self, display_name: &str) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::PATCH,
            &org_path("me"),
            Some(&DisplayNameBody { display_name }),
            None,
        )
        .await
    }
}

pub struct MeSessions<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> MeSessions<'a> {
    /// `GET /me/sessions`.
    pub async fn list(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("me/sessions"),
            None,
            None,
        )
        .await
    }

    /// Revoke one session: `POST /auth/sessions/<id>/revoke`. The result
    /// carries `{revoked}`.
    pub async fn revoke(&self, session_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::POST,
            &org_path(&format!("auth/sessions/{}/revoke", enc(session_id))),
            None,
            None,
        )
        .await
    }

    /// Revoke every session of the caller: `POST /me/sessions/revoke-all`. The
    /// result carries `{revoked}` (the count).
    pub async fn revoke_all(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::POST,
            &org_path("me/sessions/revoke-all"),
            None,
            None,
        )
        .await
    }
}

pub struct MeApiTokens<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> MeApiTokens<'a> {
    /// `GET /me/api-tokens`.
    pub async fn list(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("me/api-tokens"),
            None,
            None,
        )
        .await
    }

    /// Mint an API token: `POST /me/api-tokens`. The result carries the token
    /// record plus its one-time `plaintext`.
    pub async fn create(&self, input: CreateApiTokenInput<'_>) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::POST,
            &org_path("me/api-tokens"),
            Some(&input),
            None,
        )
        .await
    }

    /// Revoke an API token: `DELETE /me/api-tokens/<id>`.
    pub async fn delete(&self, token_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &org_path(&format!("me/api-tokens/{}", enc(token_id))),
            None,
            None,
        )
        .await
    }
}
