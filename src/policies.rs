//! Policy bundle lifecycle namespace — `client.policies()`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Policy bundle record as the gateway returns it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyBundle {
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub status: String,
    #[serde(default)]
    pub policy: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Value>,
    pub created_by: String,
    pub created_at: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub superseded_at: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub source: String,
    /// `"visual"`, `"json"` or `"unspecified"`.
    #[serde(default)]
    pub editor_mode: String,
    #[serde(default)]
    pub superseded_reason: Option<String>,
}

/// Body of `policies().create`. `policy` is the policy document: sections
/// keyed by resource kind (`collections`, `storage`, `tools`, `functions`,
/// `secrets`, `streams`, `analytics`); other keys are allowed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePolicyInput<'a> {
    pub policy: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<&'a Value>,
    /// `"visual"`, `"json"` or `"unspecified"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_mode: Option<&'a str>,
}

/// Body of `policies().update`: replaces a draft bundle's policy document.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePolicyInput<'a> {
    pub policy: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_mode: Option<&'a str>,
}

/// Body of `policies().validate`.
#[derive(Debug, Serialize)]
pub struct ValidatePolicyInput<'a> {
    pub policy: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<&'a Value>,
}

/// One error or warning of a validated policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyDiagnostic {
    pub rule: String,
    pub message: String,
}

/// Result of `policies().validate`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidatePolicyResult {
    pub valid: bool,
    #[serde(default)]
    pub errors: Vec<PolicyDiagnostic>,
    #[serde(default)]
    pub warnings: Vec<PolicyDiagnostic>,
}

/// Policies namespace — `client.policies()`.
pub struct PoliciesNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> PoliciesNamespace<'a> {
    /// Read the currently-active policy bundle (None data when none active).
    pub async fn current(&self) -> Result<Response<Option<PolicyBundle>>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<Option<PolicyBundle>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "policy"),
            None,
            None,
        )
        .await
    }

    pub async fn list(&self, status: Option<&str>) -> Result<Response<Vec<PolicyBundle>>, KError> {
        let slug = require_project(self.config, "policies")?;
        let q: Vec<(String, String)> = status
            .map(|s| vec![("status".into(), s.into())])
            .unwrap_or_default();
        request::<Vec<PolicyBundle>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "policies"),
            None,
            Some(&q),
        )
        .await
    }

    /// Create a draft policy bundle: `POST /@<slug>/policies`.
    pub async fn create(
        &self,
        input: CreatePolicyInput<'_>,
    ) -> Result<Response<PolicyBundle>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<PolicyBundle, _>(
            self.config,
            Method::POST,
            &project_path(slug, "policies"),
            Some(&input),
            None,
        )
        .await
    }

    /// Lint a candidate policy document without persisting it.
    pub async fn validate(
        &self,
        input: ValidatePolicyInput<'_>,
    ) -> Result<Response<ValidatePolicyResult>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<ValidatePolicyResult, _>(
            self.config,
            Method::POST,
            &project_path(slug, "policies/validate"),
            Some(&input),
            None,
        )
        .await
    }

    pub async fn get(&self, bundle_id: &str) -> Result<Response<PolicyBundle>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<PolicyBundle, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("policies/{}", enc(bundle_id))),
            None,
            None,
        )
        .await
    }

    /// Replace a draft bundle's policy document. Published bundles cannot be
    /// patched (publish, then republish).
    pub async fn update(
        &self,
        bundle_id: &str,
        patch: UpdatePolicyInput<'_>,
    ) -> Result<Response<PolicyBundle>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<PolicyBundle, _>(
            self.config,
            Method::PATCH,
            &project_path(slug, &format!("policies/{}", enc(bundle_id))),
            Some(&patch),
            None,
        )
        .await
    }

    pub async fn delete(&self, bundle_id: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("policies/{}", enc(bundle_id))),
            None,
            None,
        )
        .await
    }

    pub async fn publish(&self, bundle_id: &str) -> Result<Response<PolicyBundle>, KError> {
        let slug = require_project(self.config, "policies")?;
        request::<PolicyBundle, ()>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("policies/{}/publish", enc(bundle_id))),
            None,
            None,
        )
        .await
    }
}
