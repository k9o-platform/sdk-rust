//! Approvals namespaces: `client.approvals()` (the pending-change queue) and
//! `client.approval_policies()` (the rules that decide which writes are queued).
//!
//! Wire mapping (project plane):
//!   - `approvals().list(..)`       -> `GET    /@<slug>/pending-approvals`
//!   - `approvals().get(id)`        -> `GET    /@<slug>/pending-approvals/<id>`
//!   - `approvals().approve(id, ..)`-> `POST   /@<slug>/pending-approvals/<id>/approve`
//!   - `approvals().reject(id, ..)` -> `POST   /@<slug>/pending-approvals/<id>/reject`
//!   - `approvals().bulk(..)`       -> `POST   /@<slug>/pending-approvals/bulk`
//!   - `approvals().comments().add(id, ..)`  -> `POST /@<slug>/pending-approvals/<id>/comments`
//!   - `approvals().comments().list(id, ..)` -> `GET  /@<slug>/pending-approvals/<id>/comments`
//!   - `approval_policies().list()` -> `GET    /@<slug>/approval-policies`
//!   - `approval_policies().create(..)` -> `POST /@<slug>/approval-policies`
//!   - `approval_policies().delete(id)` -> `DELETE /@<slug>/approval-policies/<id>`
//!
//! `approvals().list` returns the newest changes first, `limit` (1 to 200,
//! default 50) at a time. `meta.cursor.next` of the answer is the `cursor` for
//! the next page, and `None` on the last page. Project admins and auditors see
//! the whole queue; other members see only the changes they proposed.

use crate::envelope::Response;
use crate::http::{request, request_bare_query, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// A change waiting for (or past) a decision. `status` is `pending_approval`,
/// `approved`, `rejected`, `expired` or `redeemed`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChange {
    /// `apr_<base62>`.
    pub id: String,
    pub status: String,
    pub collection: String,
    /// `insert`, `update` or `delete`.
    pub operation: String,
    pub maker_id: String,
    /// The proposed record for a data write; for other kinds of change, a
    /// description of the action.
    #[serde(default)]
    pub proposed_payload: Value,
    #[serde(default)]
    pub target_record_id: Option<String>,
    #[serde(default)]
    pub approver_id: Option<String>,
    #[serde(default)]
    pub decided_at: Option<String>,
    #[serde(default)]
    pub decision_reason: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// What a write answers with when it was queued instead of applied (HTTP 202):
/// the change as it was proposed. The decision fields (`approver_id`,
/// `decided_at`, `decision_reason`, `created_at`, `expires_at`) are not part of
/// this answer; read the full change with `approvals().get(id)`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedChange {
    /// `apr_<base62>`; pass it to `approvals().get`, `approve` or `reject`.
    pub id: String,
    /// Always `pending_approval`.
    pub status: String,
    pub collection: String,
    /// `insert`, `update` or `delete`.
    pub operation: String,
    pub maker_id: String,
    /// Insert: the record. Update: the patch. Delete: `{}`.
    #[serde(default)]
    pub proposed_payload: Value,
    /// The record the change targets; `None` for an insert.
    #[serde(default)]
    pub target_record_id: Option<String>,
}

/// Options of `approvals().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListApprovalsOptions<'a> {
    /// Only changes in this status; `None` for all.
    pub status: Option<&'a str>,
    /// 1 to 200; the gateway defaults to 50.
    pub limit: Option<u32>,
    /// `meta.cursor.next` of the previous page. A cursor is tied to the
    /// `status` it was issued for; a malformed or mismatched one fails with
    /// `K9O_INVALID_CURSOR` (HTTP 400).
    pub cursor: Option<&'a str>,
}

/// A comment on a pending change. A comment is never a decision.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalComment {
    /// `acm_` id.
    pub id: String,
    /// The pending change the comment is on.
    pub case_id: String,
    /// The author's opaque user id.
    pub author_id: String,
    /// `everyone` or `approvers_only`; fixed when the comment is posted.
    pub visibility: String,
    /// Plain text. Render it as text, never as HTML.
    pub body: String,
    /// When the comment was posted (date-time).
    pub created_at: String,
}

/// One page of comments from `approvals().comments().list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApprovalCommentPage {
    /// Oldest first; only the comments the caller may see.
    #[serde(rename = "data")]
    pub comments: Vec<ApprovalComment>,
    /// Pass as `cursor` for the next page; `None` on the last page.
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
}

/// Input of `approvals().comments().add`.
#[derive(Debug, Clone, Copy)]
pub struct AddApprovalCommentInput<'a> {
    /// Plain text, 1 to 4000 characters.
    pub body: &'a str,
    /// `everyone` or `approvers_only`; required, and fixed once posted.
    pub visibility: &'a str,
}

/// Options of `approvals().comments().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListApprovalCommentsOptions<'a> {
    /// 1 to 200; the gateway defaults to 50.
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<&'a str>,
}

/// Input of `approvals().approve`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproveInput<'a> {
    /// Optional note stored with the decision.
    pub reason: Option<&'a str>,
}

/// Input of `approvals().reject`.
#[derive(Debug, Clone, Copy)]
pub struct RejectInput<'a> {
    /// Required, non-empty.
    pub reason: &'a str,
}

/// Input of `approvals().bulk`.
#[derive(Debug, Clone, Copy)]
pub struct BulkApprovalsInput<'a> {
    /// `approve` or `reject`.
    pub action: &'a str,
    /// A predicate object over each pending change, for example
    /// `{"eq": ["change.table", "orders"]}`.
    pub criteria: &'a Value,
    /// Required when `action` is `reject`.
    pub reason: Option<&'a str>,
}

/// One row that `approvals().bulk` matched but could not decide.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BulkApprovalFailure {
    /// The pending change that was not decided.
    pub id: String,
    /// Why, for example `K9O_STATE_CONFLICT`. Treat it as an open string.
    pub code: String,
    /// Human-readable explanation of the failure.
    pub reason: String,
}

/// Result of `approvals().bulk`. Every matched row is in exactly one of
/// `affected_ids` or `failed`. The call answers HTTP 200 even when rows fail
/// (and nothing is rolled back), so compare `affected_ids` with the set you
/// meant to decide.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkApprovalsResult {
    pub attestation_id: String,
    pub action: String,
    pub affected_count: u64,
    #[serde(default)]
    pub affected_ids: Vec<String>,
    /// Rows that matched but could not be decided; empty when none failed.
    #[serde(default)]
    pub failed: Vec<BulkApprovalFailure>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A rule: writes to `collection` with `operation` that match `predicate` get `action`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPolicy {
    /// `apo_<base62>`.
    pub id: String,
    pub collection: String,
    pub operation: String,
    #[serde(default)]
    pub predicate: Value,
    /// `auto_approve` or `queue`.
    pub action: String,
    #[serde(default)]
    pub authored_by: String,
    #[serde(default)]
    pub created_at: String,
}

/// Input of `approval_policies().create`.
#[derive(Debug, Clone, Copy)]
pub struct CreateApprovalPolicyInput<'a> {
    /// `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$`.
    pub collection: &'a str,
    /// `insert`, `update` or `delete`.
    pub operation: &'a str,
    /// `auto_approve` or `queue`.
    pub action: &'a str,
    /// A predicate object.
    pub predicate: &'a Value,
}

/// Result of `approval_policies().delete`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApprovalPolicyDeleted {
    pub id: String,
}

fn invalid(message: &str) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

pub(crate) fn check_limit(limit: Option<u32>) -> Result<(), KError> {
    match limit {
        Some(n) if !(1..=200).contains(&n) => {
            Err(invalid("limit must be a whole number from 1 to 200").with_context("limit", n))
        }
        _ => Ok(()),
    }
}

fn require_id(id: &str, what: &str) -> Result<(), KError> {
    if id.is_empty() {
        return Err(invalid(&format!("{what} id must be a non-empty string")));
    }
    Ok(())
}

fn require_reason(reason: Option<&str>) -> Result<&str, KError> {
    match reason {
        Some(r) if !r.trim().is_empty() => Ok(r),
        _ => Err(invalid("reason is required and must be a non-empty string")),
    }
}

fn require_predicate<'v>(value: &'v Value, field: &str) -> Result<&'v Value, KError> {
    if value.is_object() {
        Ok(value)
    } else {
        Err(invalid(&format!("{field} must be a predicate object")))
    }
}

fn valid_collection(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let first = bytes[0];
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Approvals namespace: `client.approvals()`.
pub struct ApprovalsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

/// Comments on pending changes: `client.approvals().comments()`.
pub struct ApprovalCommentsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl ApprovalCommentsNamespace<'_> {
    /// Add a comment to a pending change. `body` is plain text of 1 to 4000
    /// characters; `visibility` is `everyone` or `approvers_only` and cannot be
    /// changed afterwards. There is no edit or delete: a correction is a new
    /// comment. Needs a session. Errors: `K9O_APPROVAL_COMMENT_NOT_ELIGIBLE`
    /// (403), `K9O_APPROVAL_COMMENT_CLOSED` (409), `K9O_INVALID_FIELD_VALUE`
    /// (400), `K9O_NOT_FOUND` (404), `K9O_RBAC_DENY` (403).
    pub async fn add(
        &self,
        pending_id: &str,
        input: AddApprovalCommentInput<'_>,
    ) -> Result<Response<ApprovalComment>, KError> {
        require_id(pending_id, "approval")?;
        let len = input.body.chars().count();
        if !(1..=4000).contains(&len) {
            return Err(invalid("body must be 1 to 4000 characters").with_context("length", len));
        }
        if input.visibility != "everyone" && input.visibility != "approvers_only" {
            return Err(invalid(
                "visibility must be \"everyone\" or \"approvers_only\"",
            ));
        }
        let slug = require_project(self.config, "approvals")?;
        let body = json!({ "body": input.body, "visibility": input.visibility });
        request::<ApprovalComment, Value>(
            self.config,
            Method::POST,
            &project_path(
                slug,
                &format!("pending-approvals/{}/comments", enc(pending_id)),
            ),
            Some(&body),
            None,
        )
        .await
    }

    /// List the comments on a pending change, oldest first, one page at a time:
    /// only the ones the caller may see. Pass `next_cursor` as `cursor` for the
    /// next page. Errors: `K9O_RBAC_DENY` (403), `K9O_NOT_FOUND` (404),
    /// `K9O_INVALID_FIELD_VALUE` (400).
    pub async fn list(
        &self,
        pending_id: &str,
        opts: ListApprovalCommentsOptions<'_>,
    ) -> Result<Response<ApprovalCommentPage>, KError> {
        require_id(pending_id, "approval")?;
        check_limit(opts.limit)?;
        let slug = require_project(self.config, "approvals")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(n) = opts.limit {
            q.push(("limit".into(), n.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request_bare_query::<ApprovalCommentPage>(
            self.config,
            Method::GET,
            &project_path(
                slug,
                &format!("pending-approvals/{}/comments", enc(pending_id)),
            ),
            Some(&q),
        )
        .await
    }
}

impl ApprovalsNamespace<'_> {
    /// Comments on pending changes: `add` and `list`.
    pub fn comments(&self) -> ApprovalCommentsNamespace<'_> {
        ApprovalCommentsNamespace {
            config: self.config,
        }
    }

    /// List pending changes, newest first, one page at a time. Pass
    /// `meta.cursor.next` as `cursor` for the next page.
    pub async fn list(
        &self,
        opts: ListApprovalsOptions<'_>,
    ) -> Result<Response<Vec<PendingChange>>, KError> {
        check_limit(opts.limit)?;
        let slug = require_project(self.config, "approvals")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(s) = opts.status {
            q.push(("status".into(), s.into()));
        }
        if let Some(n) = opts.limit {
            q.push(("limit".into(), n.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<PendingChange>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "pending-approvals"),
            None,
            Some(&q),
        )
        .await
    }

    /// One pending change. Members who are not queue admins can read only their own.
    pub async fn get(&self, id: &str) -> Result<Response<PendingChange>, KError> {
        require_id(id, "approval")?;
        let slug = require_project(self.config, "approvals")?;
        request::<PendingChange, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("pending-approvals/{}", enc(id))),
            None,
            None,
        )
        .await
    }

    /// Approve a pending change. Read `status` on the answer: `pending_approval`
    /// means the approval was recorded but more approvers are needed.
    pub async fn approve(
        &self,
        id: &str,
        input: ApproveInput<'_>,
    ) -> Result<Response<PendingChange>, KError> {
        require_id(id, "approval")?;
        let slug = require_project(self.config, "approvals")?;
        let body = match input.reason {
            Some(r) => json!({ "reason": r }),
            None => json!({}),
        };
        request::<PendingChange, Value>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("pending-approvals/{}/approve", enc(id))),
            Some(&body),
            None,
        )
        .await
    }

    /// Reject a pending change. `reason` is required.
    pub async fn reject(
        &self,
        id: &str,
        input: RejectInput<'_>,
    ) -> Result<Response<PendingChange>, KError> {
        let reason = require_reason(Some(input.reason))?;
        require_id(id, "approval")?;
        let slug = require_project(self.config, "approvals")?;
        request::<PendingChange, Value>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("pending-approvals/{}/reject", enc(id))),
            Some(&json!({ "reason": reason })),
            None,
        )
        .await
    }

    /// Approve or reject every pending change matching `criteria`. The call
    /// answers 200 even when some rows fail: those are listed in `failed`, so
    /// compare `affected_ids` with what you intended.
    pub async fn bulk(
        &self,
        input: BulkApprovalsInput<'_>,
    ) -> Result<Response<BulkApprovalsResult>, KError> {
        if input.action != "approve" && input.action != "reject" {
            return Err(invalid("action must be \"approve\" or \"reject\""));
        }
        let criteria = require_predicate(input.criteria, "criteria")?;
        let mut body = Map::new();
        body.insert("action".into(), json!(input.action));
        body.insert("criteria".into(), criteria.clone());
        if input.action == "reject" {
            body.insert("reason".into(), json!(require_reason(input.reason)?));
        } else if let Some(r) = input.reason {
            body.insert("reason".into(), json!(r));
        }
        let slug = require_project(self.config, "approvals")?;
        request::<BulkApprovalsResult, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "pending-approvals/bulk"),
            Some(&Value::Object(body)),
            None,
        )
        .await
    }
}

/// Approval policies namespace: `client.approval_policies()`.
pub struct ApprovalPoliciesNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl ApprovalPoliciesNamespace<'_> {
    /// All policies, oldest first. The first matching policy for a write decides.
    pub async fn list(&self) -> Result<Response<Vec<ApprovalPolicy>>, KError> {
        let slug = require_project(self.config, "approval_policies")?;
        request::<Vec<ApprovalPolicy>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "approval-policies"),
            None,
            None,
        )
        .await
    }

    /// Create a policy. Identical policies are not de-duplicated.
    pub async fn create(
        &self,
        input: CreateApprovalPolicyInput<'_>,
    ) -> Result<Response<ApprovalPolicy>, KError> {
        if !valid_collection(input.collection) {
            return Err(
                invalid("collection must match ^[a-zA-Z_][a-zA-Z0-9_]{0,62}$")
                    .with_context("collection", input.collection),
            );
        }
        if !matches!(input.operation, "insert" | "update" | "delete") {
            return Err(invalid(
                "operation must be \"insert\", \"update\" or \"delete\"",
            ));
        }
        if !matches!(input.action, "auto_approve" | "queue") {
            return Err(invalid("action must be \"auto_approve\" or \"queue\""));
        }
        let predicate = require_predicate(input.predicate, "predicate")?;
        let slug = require_project(self.config, "approval_policies")?;
        let body = json!({
            "collection": input.collection,
            "operation": input.operation,
            "action": input.action,
            "predicate": predicate,
        });
        request::<ApprovalPolicy, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "approval-policies"),
            Some(&body),
            None,
        )
        .await
    }

    /// Delete a policy; answers `{ id }`.
    pub async fn delete(&self, id: &str) -> Result<Response<ApprovalPolicyDeleted>, KError> {
        require_id(id, "policy")?;
        let slug = require_project(self.config, "approval_policies")?;
        request::<ApprovalPolicyDeleted, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("approval-policies/{}", enc(id))),
            None,
            None,
        )
        .await
    }
}
