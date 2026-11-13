//! Project lifecycle namespace — `client.projects.*`.
//!
//! All four operations are org-plane (`/projects`, `/projects/<slug>`); the
//! client needs an org context. Input property names are the gateway's body
//! property names.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::url_grammar::{enc, org_path, validate_slug};
use reqwest::Method;
use serde::{Deserialize, Serialize};

/// Project record as the gateway returns it. Rust idiomatic shape (snake_case
/// via serde rename on wire camelCase).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub org_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub product_scope: String,
    #[serde(default)]
    pub region: String,
    pub custody_mode: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub deleted_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub soft_delete_retention_days: Option<i64>,
    #[serde(default)]
    pub processing_contexts: Vec<String>,
}

/// Locale of a showroom project.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowroomLocale<'a> {
    pub code: &'a str,
    pub currency: &'a str,
    pub money_scale: u32,
}

/// Body of `projects().create`; property names are the gateway's. Only
/// `display_name` is required.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectInput<'a> {
    /// Display name (1 to 128 characters). Required.
    pub display_name: &'a str,
    /// URL slug (lowercase); the gateway derives one from `display_name` when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<&'a str>,
    /// `"core"` or `"kibo"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_scope: Option<&'a str>,
    /// Region code of the project's data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_project_region: Option<&'a str>,
    /// `"PLATFORM_MANAGED"`, `"SKR"`, `"BYOK"` or `"HYOK"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custody_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_contexts: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_seam_confined: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub showroom_vertical: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub showroom_locale: Option<ShowroomLocale<'a>>,
    /// Makes a retried create return the first result instead of a second project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<&'a str>,
}

/// Body of `projects().update`: only the fields set are sent.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProjectInput<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_scope: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_delete_retention_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_contexts: Option<Vec<&'a str>>,
}

pub struct ProjectsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> ProjectsNamespace<'a> {
    /// List projects accessible to the caller: `GET /projects`.
    pub async fn list(&self) -> Result<Response<Vec<Project>>, KError> {
        request::<Vec<Project>, ()>(self.config, Method::GET, &org_path("projects"), None, None)
            .await
    }

    /// Create a new project under the caller's active org: `POST /projects`.
    pub async fn create(&self, input: CreateProjectInput<'_>) -> Result<Response<Project>, KError> {
        if let Some(slug) = input.slug {
            validate_slug(slug, "project")?;
        }
        request::<Project, _>(
            self.config,
            Method::POST,
            &org_path("projects"),
            Some(&input),
            None,
        )
        .await
    }

    /// Get a project by slug: `GET /projects/<slug>`.
    pub async fn get(&self, slug: &str) -> Result<Response<Project>, KError> {
        validate_slug(slug, "project")?;
        request::<Project, ()>(
            self.config,
            Method::GET,
            &org_path(&format!("projects/{}", enc(slug))),
            None,
            None,
        )
        .await
    }

    /// Update a project's mutable fields: `PATCH /projects/<slug>`.
    pub async fn update(
        &self,
        slug: &str,
        patch: UpdateProjectInput<'_>,
    ) -> Result<Response<Project>, KError> {
        validate_slug(slug, "project")?;
        request::<Project, _>(
            self.config,
            Method::PATCH,
            &org_path(&format!("projects/{}", enc(slug))),
            Some(&patch),
            None,
        )
        .await
    }
}
