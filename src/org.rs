//! Org-plane bound namespace — `client.org()`.
//!
//! Singular bound-org namespace (distinct from `client.orgs()` collection):
//! members / groups / smtp / audit_log / gate (roles + idp_providers) +
//! default_llm. Org-plane routes live on the org host without the
//! `@<projectSlug>/` prefix.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::url_grammar::{enc, org_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize)]
struct SetDefaultLlmBody<'a> {
    #[serde(rename = "modelId")]
    model_id: &'a str,
}

#[derive(Serialize)]
struct InviteBody<'a> {
    email: &'a str,
    role: &'a str,
}

/// The org-default LLM record (`GET /default-llm`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgDefaultLlm {
    pub org_id: String,
    pub default_model_id: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub updated_by: Option<String>,
}

/// Result of setting the org-default LLM (`PATCH /default-llm`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgDefaultLlmSet {
    pub org_id: String,
    pub default_model_id: String,
}

/// Body of `org().gate().roles().grant`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantRoleInput<'a> {
    pub user_id: &'a str,
    /// `"ORG_OWNER"`, `"ORG_ADMIN"`, `"ORG_BILLING_ADMIN"`, `"ORG_AUDITOR"` or `"ORG_MEMBER"`.
    pub role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevokeBody<'a> {
    decision_reason: &'a str,
}

/// Body of `org().gate().idp_providers().create`. `client_secret_ref` carries the
/// OAuth client secret itself (the name is kept for compatibility); the gateway encrypts it and never returns it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateIdpInput<'a> {
    /// Provider kind: `"google"`, `"github"`, `"microsoft"`, `"oidc"`, `"saml"`,
    /// `"scim"`, `"ldap"`, `"active_directory"`, `"apple"`, `"facebook"` or `"twitter"`.
    pub idp_provider: &'a str,
    pub display_name: &'a str,
    pub client_id: &'a str,
    pub client_secret_ref: &'a str,
    /// `"free_tier_ok"` or `"ridge_plus"`.
    pub tier_gating: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribute_map: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Body of `org().gate().idp_providers().configure`: only the fields set are sent.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateIdpInput<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret_ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribute_map: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Input for `org().smtp().set()`: the org SMTP configuration as the gateway
/// takes it in the `smtpConfig` field of `PATCH /org`. The password is never
/// sent: `password_ref` references a stored secret.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSmtpInput<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    /// Reference to the stored SMTP password (an opaque token).
    pub password_ref: &'a str,
    pub from_address: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_name: Option<&'a str>,
}

#[derive(Serialize)]
struct SmtpConfigBody<'a> {
    #[serde(rename = "smtpConfig")]
    smtp_config: &'a SetSmtpInput<'a>,
}

#[derive(Serialize)]
struct SmtpUnsetBody {
    #[serde(rename = "smtpConfig")]
    smtp_config: Option<()>,
}

#[derive(Serialize)]
struct TestEmailBody<'a> {
    to: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<&'a str>,
}

/// Filters of `org().audit_log().query`; the org-plane route reads only these two.
#[derive(Debug, Default, Clone)]
pub struct OrgAuditQueryOpts<'a> {
    /// Filter by event type.
    pub event_type: Option<&'a str>,
    /// Page size (1 to 500, default 100).
    pub limit: Option<u32>,
}

/// Org namespace — `client.org()`.
pub struct OrgNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> OrgNamespace<'a> {
    pub fn members(&self) -> OrgMembers<'a> {
        OrgMembers {
            config: self.config,
        }
    }
    pub fn groups(&self) -> OrgGroups<'a> {
        OrgGroups {
            config: self.config,
        }
    }
    pub fn smtp(&self) -> OrgSmtp<'a> {
        OrgSmtp {
            config: self.config,
        }
    }
    pub fn audit_log(&self) -> OrgAuditLog<'a> {
        OrgAuditLog {
            config: self.config,
        }
    }
    pub fn gate(&self) -> OrgGate<'a> {
        OrgGate {
            config: self.config,
        }
    }

    /// Read the current org: `GET /org`.
    pub async fn get(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(self.config, Method::GET, &org_path("org"), None, None).await
    }

    /// Read the org-default LLM: `GET /default-llm`. Any accepted member may read it.
    pub async fn default_llm_get(&self) -> Result<Response<OrgDefaultLlm>, KError> {
        request::<OrgDefaultLlm, ()>(
            self.config,
            Method::GET,
            &org_path("default-llm"),
            None,
            None,
        )
        .await
    }

    /// Set the org-default LLM: `PATCH /default-llm` with `{modelId}`
    /// (ORG_ADMIN or ORG_OWNER; the model must be available to the org's tier).
    pub async fn default_llm_set(
        &self,
        model_id: &str,
    ) -> Result<Response<OrgDefaultLlmSet>, KError> {
        request::<OrgDefaultLlmSet, _>(
            self.config,
            Method::PATCH,
            &org_path("default-llm"),
            Some(&SetDefaultLlmBody { model_id }),
            None,
        )
        .await
    }
}

pub struct OrgMembers<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgMembers<'a> {
    /// `GET /members`, optionally filtered by status.
    pub async fn list(&self, status: Option<&str>) -> Result<Response<Value>, KError> {
        let q: Vec<(String, String)> = status
            .map(|s| vec![("status".into(), s.into())])
            .unwrap_or_default();
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("members"),
            None,
            Some(&q),
        )
        .await
    }

    /// Invite a member by email: `POST /members`.
    pub async fn invite(&self, email: &str, role: &str) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::POST,
            &org_path("members"),
            Some(&InviteBody { email, role }),
            None,
        )
        .await
    }

    /// Remove a member: `DELETE /members/<userId>`.
    pub async fn remove(&self, user_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &org_path(&format!("members/{}", enc(user_id))),
            None,
            None,
        )
        .await
    }
}

pub struct OrgGroups<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgGroups<'a> {
    /// `GET /groups`.
    pub async fn list(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(self.config, Method::GET, &org_path("groups"), None, None).await
    }

    /// `GET /groups/<groupId>`.
    pub async fn get(&self, group_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path(&format!("groups/{}", enc(group_id))),
            None,
            None,
        )
        .await
    }
}

pub struct OrgAuditLog<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgAuditLog<'a> {
    /// Read the org audit log: `GET /audit-log` (query `type`, `limit`).
    pub async fn query(&self, opts: OrgAuditQueryOpts<'_>) -> Result<Response<Value>, KError> {
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(t) = opts.event_type {
            q.push(("type".into(), t.into()));
        }
        if let Some(l) = opts.limit {
            q.push(("limit".into(), l.to_string()));
        }
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("audit-log"),
            None,
            Some(&q),
        )
        .await
    }
}

pub struct OrgGate<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgGate<'a> {
    pub fn roles(&self) -> OrgRoles<'a> {
        OrgRoles {
            config: self.config,
        }
    }
    pub fn idp_providers(&self) -> OrgIdpProviders<'a> {
        OrgIdpProviders {
            config: self.config,
        }
    }
}

pub struct OrgRoles<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgRoles<'a> {
    /// List role grants: `GET /role-grants`.
    pub async fn list(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("role-grants"),
            None,
            None,
        )
        .await
    }

    /// Grant an org role: `POST /role-grants`.
    pub async fn grant(&self, input: GrantRoleInput<'_>) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::POST,
            &org_path("role-grants"),
            Some(&input),
            None,
        )
        .await
    }

    /// Read one grant: `GET /role-grants/<id>`.
    pub async fn get(&self, grant_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path(&format!("role-grants/{}", enc(grant_id))),
            None,
            None,
        )
        .await
    }

    /// Revoke a grant (soft delete): `DELETE /role-grants/<id>`. A
    /// `decision_reason` is sent as the request body `{decisionReason}`.
    pub async fn revoke(
        &self,
        grant_id: &str,
        decision_reason: Option<&str>,
    ) -> Result<Response<Value>, KError> {
        let path = org_path(&format!("role-grants/{}", enc(grant_id)));
        match decision_reason {
            Some(r) => {
                request::<Value, _>(
                    self.config,
                    Method::DELETE,
                    &path,
                    Some(&RevokeBody { decision_reason: r }),
                    None,
                )
                .await
            }
            None => request::<Value, ()>(self.config, Method::DELETE, &path, None, None).await,
        }
    }
}

pub struct OrgIdpProviders<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgIdpProviders<'a> {
    /// `GET /identity-providers`.
    pub async fn list(&self) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path("identity-providers"),
            None,
            None,
        )
        .await
    }

    /// `POST /identity-providers`.
    pub async fn create(&self, input: CreateIdpInput<'_>) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::POST,
            &org_path("identity-providers"),
            Some(&input),
            None,
        )
        .await
    }

    /// `GET /identity-providers/<id>`.
    pub async fn get(&self, idp_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::GET,
            &org_path(&format!("identity-providers/{}", enc(idp_id))),
            None,
            None,
        )
        .await
    }

    /// Update a provider's settings or enable/disable it: `PATCH /identity-providers/<id>`.
    pub async fn configure(
        &self,
        idp_id: &str,
        patch: UpdateIdpInput<'_>,
    ) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::PATCH,
            &org_path(&format!("identity-providers/{}", enc(idp_id))),
            Some(&patch),
            None,
        )
        .await
    }

    /// `DELETE /identity-providers/<id>`.
    pub async fn delete(&self, idp_id: &str) -> Result<Response<Value>, KError> {
        request::<Value, ()>(
            self.config,
            Method::DELETE,
            &org_path(&format!("identity-providers/{}", enc(idp_id))),
            None,
            None,
        )
        .await
    }
}

pub struct OrgSmtp<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> OrgSmtp<'a> {
    /// Set or update the org SMTP configuration: `PATCH /org` with `{smtpConfig}`.
    pub async fn set(&self, input: &SetSmtpInput<'_>) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::PATCH,
            &org_path("org"),
            Some(&SmtpConfigBody { smtp_config: input }),
            None,
        )
        .await
    }

    /// Send a diagnostic email: `POST /org/smtp/test-email` with `{to, subject?}`.
    pub async fn test(&self, to: &str, subject: Option<&str>) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::POST,
            &org_path("org/smtp/test-email"),
            Some(&TestEmailBody { to, subject }),
            None,
        )
        .await
    }

    /// Clear the org-level SMTP configuration: `PATCH /org` with `{smtpConfig: null}`.
    pub async fn unset(&self) -> Result<Response<Value>, KError> {
        request::<Value, _>(
            self.config,
            Method::PATCH,
            &org_path("org"),
            Some(&SmtpUnsetBody { smtp_config: None }),
            None,
        )
        .await
    }
}
