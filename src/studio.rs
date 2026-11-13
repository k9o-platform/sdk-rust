//! Studio namespace — `client.studio()` — workflows and dashboards.
//!
//! Rust idiomatic: snake_case methods; returns
//! `Result<Response<T>, KError>`; serde rename handles wire camelCase.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Workflow record (`_sys.workflows` row).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub status: String,
    pub trigger_type: String,
    #[serde(default)]
    pub trigger_config: Value,
    #[serde(default)]
    pub actions: Vec<Value>,
    #[serde(default)]
    pub rbac_binding: Value,
    #[serde(default)]
    pub authored_by: String,
    #[serde(default)]
    pub authored_at: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Workflow run record (`_sys.workflow_runs` row).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRun {
    pub id: String,
    pub workflow_id: String,
    pub project_id: String,
    pub status: String,
    #[serde(default)]
    pub trigger_context: Value,
    #[serde(default)]
    pub per_step_status: Vec<Value>,
    #[serde(default)]
    pub cost_accounting_total: f64,
    #[serde(default)]
    pub error_context: Option<Value>,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub fired_by: String,
}

/// Dashboard record (`_sys.dashboards` row).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dashboard {
    pub id: String,
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub status: String,
    #[serde(default)]
    pub layout_grid: Value,
    #[serde(default)]
    pub widgets: Vec<Value>,
    #[serde(default)]
    pub filter_config: Value,
    #[serde(default)]
    pub pinned_to_workspace: bool,
    #[serde(default)]
    pub rbac_binding: Value,
    #[serde(default)]
    pub authored_by: String,
    #[serde(default)]
    pub authored_at: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Workflow runs sub-namespace — `client.studio().workflows().runs()`.
pub struct WorkflowRunsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> WorkflowRunsNamespace<'a> {
    pub async fn list(
        &self,
        workflow_id: &str,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<Response<Vec<WorkflowRun>>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(l) = limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor".into(), c.into()));
        }
        let path = project_path(slug, &format!("workflows/{}/runs", enc(workflow_id)));
        request::<Vec<WorkflowRun>, ()>(self.config, Method::GET, &path, None, Some(&q)).await
    }

    pub async fn read(
        &self,
        workflow_id: &str,
        run_id: &str,
    ) -> Result<Response<WorkflowRun>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(
            slug,
            &format!("workflows/{}/runs/{}", enc(workflow_id), enc(run_id)),
        );
        request::<WorkflowRun, ()>(self.config, Method::GET, &path, None, None).await
    }

    pub async fn cancel(
        &self,
        workflow_id: &str,
        run_id: &str,
    ) -> Result<Response<WorkflowRun>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(
            slug,
            &format!("workflows/{}/runs/{}/cancel", enc(workflow_id), enc(run_id)),
        );
        request::<WorkflowRun, ()>(self.config, Method::POST, &path, None, None).await
    }
}

/// Workflows namespace — `client.studio().workflows()`
pub struct WorkflowsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> WorkflowsNamespace<'a> {
    pub fn runs(&self) -> WorkflowRunsNamespace<'a> {
        WorkflowRunsNamespace {
            config: self.config,
        }
    }

    pub async fn list(
        &self,
        status: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<Response<Vec<Workflow>>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(s) = status {
            q.push(("status".into(), s.into()));
        }
        if let Some(l) = limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<Workflow>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "workflows"),
            None,
            Some(&q),
        )
        .await
    }

    pub async fn create(&self, workflow: &Value) -> Result<Response<Workflow>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        request::<Workflow, _>(
            self.config,
            Method::POST,
            &project_path(slug, "workflows"),
            Some(workflow),
            None,
        )
        .await
    }

    pub async fn read(&self, workflow_id: &str) -> Result<Response<Workflow>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(slug, &format!("workflows/{}", enc(workflow_id)));
        request::<Workflow, ()>(self.config, Method::GET, &path, None, None).await
    }

    pub async fn update(
        &self,
        workflow_id: &str,
        updates: &Value,
    ) -> Result<Response<Workflow>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(slug, &format!("workflows/{}", enc(workflow_id)));
        request::<Workflow, _>(self.config, Method::PATCH, &path, Some(updates), None).await
    }

    pub async fn delete(&self, workflow_id: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(slug, &format!("workflows/{}", enc(workflow_id)));
        request::<Value, ()>(self.config, Method::DELETE, &path, None, None).await
    }

    /// Manual-trigger a run (trigger_type=manual only)
    pub async fn trigger(
        &self,
        workflow_id: &str,
        trigger_context: &Value,
    ) -> Result<Response<WorkflowRun>, KError> {
        let slug = require_project(self.config, "studio.workflows")?;
        let path = project_path(slug, &format!("workflows/{}/runs", enc(workflow_id)));
        let body = serde_json::json!({ "triggerContext": trigger_context });
        request::<WorkflowRun, _>(self.config, Method::POST, &path, Some(&body), None).await
    }
}

/// Dashboards namespace — `client.studio().dashboards()`
pub struct DashboardsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> DashboardsNamespace<'a> {
    pub async fn list(&self, status: Option<&str>) -> Result<Response<Vec<Dashboard>>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let q: Vec<(String, String)> = status
            .map(|s| vec![("status".into(), s.into())])
            .unwrap_or_default();
        request::<Vec<Dashboard>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "dashboards"),
            None,
            Some(&q),
        )
        .await
    }

    pub async fn create(&self, dashboard: &Value) -> Result<Response<Dashboard>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        request::<Dashboard, _>(
            self.config,
            Method::POST,
            &project_path(slug, "dashboards"),
            Some(dashboard),
            None,
        )
        .await
    }

    pub async fn read(&self, dashboard_id: &str) -> Result<Response<Dashboard>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let path = project_path(slug, &format!("dashboards/{}", enc(dashboard_id)));
        request::<Dashboard, ()>(self.config, Method::GET, &path, None, None).await
    }

    pub async fn update(
        &self,
        dashboard_id: &str,
        updates: &Value,
    ) -> Result<Response<Dashboard>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let path = project_path(slug, &format!("dashboards/{}", enc(dashboard_id)));
        request::<Dashboard, _>(self.config, Method::PATCH, &path, Some(updates), None).await
    }

    pub async fn delete(&self, dashboard_id: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let path = project_path(slug, &format!("dashboards/{}", enc(dashboard_id)));
        request::<Value, ()>(self.config, Method::DELETE, &path, None, None).await
    }

    pub async fn pin(&self, dashboard_id: &str) -> Result<Response<Dashboard>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let path = project_path(slug, &format!("dashboards/{}/pin", enc(dashboard_id)));
        request::<Dashboard, ()>(self.config, Method::POST, &path, None, None).await
    }

    pub async fn unpin(&self, dashboard_id: &str) -> Result<Response<Dashboard>, KError> {
        let slug = require_project(self.config, "studio.dashboards")?;
        let path = project_path(slug, &format!("dashboards/{}/unpin", enc(dashboard_id)));
        request::<Dashboard, ()>(self.config, Method::POST, &path, None, None).await
    }
}

/// Studio namespace grouping workflows + dashboards.
pub struct StudioNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> StudioNamespace<'a> {
    pub fn workflows(&self) -> WorkflowsNamespace<'a> {
        WorkflowsNamespace {
            config: self.config,
        }
    }

    pub fn dashboards(&self) -> DashboardsNamespace<'a> {
        DashboardsNamespace {
            config: self.config,
        }
    }
}
