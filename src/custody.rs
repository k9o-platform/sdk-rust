//! Custody namespace: `client.custody().status()`.
//!
//! Wire mapping (project plane):
//!   - `custody().status()` -> `GET /@<slug>/custody/status`
//!
//! Read-only. The project-admin and project-auditor roles can read it; changing custody is a Console action.
//! Errors: 403 for another project role, 404 when the caller is not a member of the project, and 502
//! `K9O_UPSTREAM_UNAVAILABLE` when the key manager cannot answer for an attached project.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most recent custody job of a project.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CustodyJob {
    /// `transition` or `external_transition`.
    pub kind: String,
    /// `pending`, `running`, `completed` or `failed`.
    pub status: String,
    /// When the job finished; `None` while it is pending or running.
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// HSM attestation. Both fields are `None` today.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HsmAttestation {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub verified_at: Option<String>,
}

/// The hardware security module behind an attached project (`provider` is `hsm`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HsmInfo {
    /// `thales-luna`, `entrust-nshield`, `aws-cloudhsm` or `None`.
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub cluster_id: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub attestation: HsmAttestation,
    /// Always `None` today.
    #[serde(default)]
    pub keys: Option<Vec<Value>>,
}

/// Where the project's keys live.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CustodyStatus {
    /// For example `PLATFORM_MANAGED`.
    pub mode: String,
    /// `true` when customer-held keys are attached.
    #[serde(default)]
    pub attached: bool,
    /// `platform`, `external_kms`, `hsm` or `None`.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub last_job: Option<CustodyJob>,
    /// `None` unless `provider` is `hsm`; a missing key reads as `None`.
    #[serde(default)]
    pub hsm: Option<HsmInfo>,
}

/// `client.custody()`.
pub struct CustodyNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl CustodyNamespace<'_> {
    /// `GET /@<slug>/custody/status` — the project's custody mode and attached key provider.
    pub async fn status(&self) -> Result<Response<CustodyStatus>, KError> {
        let slug = require_project(self.config, "custody.status")?;
        let path = project_path(slug, "custody/status");
        request::<CustodyStatus, ()>(self.config, Method::GET, &path, None, None).await
    }
}
