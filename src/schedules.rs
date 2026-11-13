//! Scheduled triggers — `client.schedules()`. A schedule runs a function on a cron timetable.
//!
//! Wire mapping (project plane):
//!   - `list(opts)`        -> `GET    /@<slug>/forge/schedules` (`function_name` is the query name)
//!   - `get(id)`           -> `GET    /@<slug>/forge/schedules/<id>`
//!   - `create(input)`     -> `POST   /@<slug>/forge/schedules`
//!   - `update(id, patch)` -> `PATCH  /@<slug>/forge/schedules/<id>`
//!   - `delete(id)`        -> `DELETE /@<slug>/forge/schedules/<id>` (204, no body)
//!   - `runs(id, opts)`    -> `GET    /@<slug>/forge/schedules/<id>/runs`
//!   - `run_now(id)`       -> `POST   /@<slug>/forge/schedules/<id>/run-now` (202)
//!
//! Lists are cursor-paginated, newest first: pass `meta.cursor.next` of one page as `cursor` of
//! the next, until it is `None`. An interval under 5 minutes, or `catchup_policy` `run_all`, can be refused with 403 `K9O_TIER_FEATURE_GATED`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A scheduled trigger.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    /// UUID.
    pub id: String,
    pub function_name: String,
    /// Five-field cron. An alias such as `@daily` is stored and returned expanded.
    pub schedule_expression: String,
    #[serde(default)]
    pub schedule_timezone: String,
    /// `skip`, `run_once` or `run_all`.
    #[serde(default)]
    pub catchup_policy: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    /// `active` when `enabled`, otherwise `paused`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub log_ok_invocations: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_invocation_at: Option<String>,
    #[serde(default)]
    pub next_invocation_at: Option<String>,
}

/// One run of a schedule.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRun {
    pub id: String,
    #[serde(default)]
    pub trigger_id: String,
    #[serde(default)]
    pub function_name: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default)]
    pub error_detail: Option<String>,
    #[serde(default)]
    pub error_output: Option<String>,
    #[serde(default)]
    pub record_id: Option<String>,
    #[serde(default)]
    pub triggered_at: String,
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// Options of `schedules().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListSchedulesOptions<'a> {
    /// `active` or `paused`.
    pub status: Option<&'a str>,
    /// Only the schedules of this function (sent as `function_name`).
    pub function_name: Option<&'a str>,
    /// 1 to 200; the gateway defaults to 50.
    pub limit: Option<u32>,
    /// `meta.cursor.next` of the previous page.
    pub cursor: Option<&'a str>,
}

/// Options of `schedules().runs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListScheduleRunsOptions<'a> {
    /// 1 to 200.
    pub limit: Option<u32>,
    pub cursor: Option<&'a str>,
}

/// Input of `schedules().create`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateScheduleInput<'a> {
    /// An existing function that is not retired.
    pub function_name: &'a str,
    /// Five-field cron (minute 0-59, hour 0-23, day 1-31, month 1-12, weekday 0-6) or
    /// `@yearly`, `@annually`, `@monthly`, `@weekly`, `@daily`, `@midnight`, `@hourly`.
    pub schedule_expression: &'a str,
    /// `UTC` (default) or `Region/City`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_timezone: Option<&'a str>,
    /// `skip` (default), `run_once` or `run_all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catchup_policy: Option<&'a str>,
    /// An existing collection, passed to the function with each run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<&'a str>,
    /// Default `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Record successful runs too (default `false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_ok_invocations: Option<bool>,
}

/// Input of `schedules().update`: set at least one field. The function cannot be changed.
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateScheduleInput<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_expression: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_timezone: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catchup_policy: Option<&'a str>,
    /// `active` resumes the schedule, `paused` stops it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_ok_invocations: Option<bool>,
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn check_limit(limit: Option<u32>) -> Result<(), KError> {
    match limit {
        Some(l) if !(1..=200).contains(&l) => {
            Err(invalid("limit must be a whole number from 1 to 200").with_context("limit", l))
        }
        _ => Ok(()),
    }
}

fn check_choice(value: Option<&str>, allowed: &[&str], field: &str) -> Result<(), KError> {
    match value {
        Some(v) if !allowed.contains(&v) => Err(invalid(format!(
            "{field} must be one of: {}",
            allowed.join(", ")
        ))
        .with_context("field", field)),
        _ => Ok(()),
    }
}

const CATCHUP: [&str; 3] = ["skip", "run_once", "run_all"];
const STATUS: [&str; 2] = ["active", "paused"];

/// Scheduled triggers namespace — `client.schedules()`.
pub struct SchedulesNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> SchedulesNamespace<'a> {
    fn path(&self, tail: &str) -> Result<String, KError> {
        let slug = require_project(self.config, "schedules")?;
        Ok(project_path(slug, &format!("forge/schedules{tail}")))
    }

    fn one(&self, id: &str, tail: &str) -> Result<String, KError> {
        if id.is_empty() {
            return Err(invalid("schedule id must be a non-empty string"));
        }
        self.path(&format!("/{}{}", enc(id), tail))
    }

    /// List schedules, newest first, one page at a time.
    pub async fn list(
        &self,
        opts: ListSchedulesOptions<'_>,
    ) -> Result<Response<Vec<Schedule>>, KError> {
        let path = self.path("")?;
        check_limit(opts.limit)?;
        check_choice(opts.status, &STATUS, "status")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(s) = opts.status {
            q.push(("status".into(), s.into()));
        }
        if let Some(f) = opts.function_name {
            q.push(("function_name".into(), f.into()));
        }
        if let Some(l) = opts.limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<Schedule>, ()>(self.config, Method::GET, &path, None, Some(&q)).await
    }

    pub async fn get(&self, id: &str) -> Result<Response<Schedule>, KError> {
        let path = self.one(id, "")?;
        request::<Schedule, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// Create a schedule. Answers 201 with the schedule.
    pub async fn create(
        &self,
        input: CreateScheduleInput<'_>,
    ) -> Result<Response<Schedule>, KError> {
        let path = self.path("")?;
        check_choice(input.catchup_policy, &CATCHUP, "catchup_policy")?;
        request::<Schedule, _>(self.config, Method::POST, &path, Some(&input), None).await
    }

    /// Change a schedule. Set at least one field of `patch`.
    pub async fn update(
        &self,
        id: &str,
        patch: UpdateScheduleInput<'_>,
    ) -> Result<Response<Schedule>, KError> {
        let path = self.one(id, "")?;
        if patch.schedule_expression.is_none()
            && patch.schedule_timezone.is_none()
            && patch.catchup_policy.is_none()
            && patch.status.is_none()
            && patch.enabled.is_none()
            && patch.log_ok_invocations.is_none()
        {
            return Err(invalid(
                "patch needs at least one of: schedule_expression, schedule_timezone, catchup_policy, status, enabled, log_ok_invocations",
            ));
        }
        check_choice(patch.catchup_policy, &CATCHUP, "catchup_policy")?;
        check_choice(patch.status, &STATUS, "status")?;
        request::<Schedule, _>(self.config, Method::PATCH, &path, Some(&patch), None).await
    }

    /// Delete a schedule. The gateway answers 204, so `data` is `()`.
    pub async fn delete(&self, id: &str) -> Result<Response<()>, KError> {
        let path = self.one(id, "")?;
        request::<(), ()>(self.config, Method::DELETE, &path, None, None).await
    }

    /// The schedule's runs, newest first, one page at a time.
    pub async fn runs(
        &self,
        id: &str,
        opts: ListScheduleRunsOptions<'_>,
    ) -> Result<Response<Vec<ScheduleRun>>, KError> {
        let path = self.one(id, "/runs")?;
        check_limit(opts.limit)?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(l) = opts.limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<ScheduleRun>, ()>(self.config, Method::GET, &path, None, Some(&q)).await
    }

    /// Start one run now, outside the timetable. The gateway answers 202; the
    /// result is returned as it sends it.
    pub async fn run_now(&self, id: &str) -> Result<Response<Value>, KError> {
        let path = self.one(id, "/run-now")?;
        request::<Value, ()>(self.config, Method::POST, &path, None, None).await
    }
}
