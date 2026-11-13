//! End users — `client.end_users()`: the people who sign in to the project's own apps.
//!
//! Wire mapping (project plane):
//!   - `list(opts)`                       -> `GET    /@<slug>/end-users`
//!   - `get(id)`                          -> `GET    /@<slug>/end-users/<id>`
//!   - `role_grants().list(opts)`         -> `GET    /@<slug>/end-user-role-grants`
//!   - `role_grants().grant(input)`       -> `POST   /@<slug>/end-user-role-grants`
//!   - `role_grants().revoke(grant_id)`   -> `DELETE /@<slug>/end-user-role-grants/<grant_id>`
//!
//! Listing and reading end users needs the project admin role. Role grants are created and
//! revoked by project admins; reading them is open to a wider set of project roles.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};

/// An end user of the project.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndUser {
    /// `eu_<base62>`.
    pub id: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub email_verified_at: Option<String>,
    /// `active`, `locked` or `deleted`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub last_signin_at: Option<String>,
    #[serde(default)]
    pub sign_in_methods: Vec<String>,
    #[serde(default)]
    pub region: Option<String>,
}

/// Options of `end_users().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListEndUsersOptions<'a> {
    /// `active`, `locked`, `deleted` or `all`. Default: active and locked users.
    pub status: Option<&'a str>,
    /// Email prefix (case-sensitive), up to 320 characters.
    pub q: Option<&'a str>,
    /// 1 to 200; the gateway defaults to 50.
    pub limit: Option<u32>,
    /// `meta.cursor.next` of the previous page.
    pub cursor: Option<&'a str>,
}

/// A role held by one end user. The role is a free string, not a fixed list.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndUserRoleGrant {
    /// `eurg_<base62>`.
    pub id: String,
    pub end_user_id: String,
    pub role: String,
    #[serde(default)]
    pub granted_at: String,
    /// The subject that granted the role.
    #[serde(default)]
    pub granted_by: String,
}

/// Options of `role_grants().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListRoleGrantsOptions<'a> {
    /// Only this end user's grants. An unknown id answers an empty list.
    pub end_user_id: Option<&'a str>,
}

/// Input of `role_grants().grant`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantEndUserRoleInput<'a> {
    /// `eu_<base62>` of an existing end user, up to 64 characters.
    pub end_user_id: &'a str,
    /// Any string, 1 to 128 characters.
    pub role: &'a str,
}

/// Result of `role_grants().revoke`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevokeRoleGrantResult {
    /// `false` when the grant does not exist or was already revoked.
    pub revoked: bool,
    #[serde(default)]
    pub revoked_at: Option<String>,
}

#[derive(Deserialize)]
struct GrantsWrapper {
    grants: Vec<EndUserRoleGrant>,
}

#[derive(Deserialize)]
struct GrantWrapper {
    grant: EndUserRoleGrant,
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn check_text(value: &str, what: &str, max: usize) -> Result<(), KError> {
    if value.is_empty() || value.chars().count() > max {
        Err(invalid(format!(
            "{what} must be a string of 1 to {max} characters"
        )))
    } else {
        Ok(())
    }
}

/// End users namespace — `client.end_users()`.
pub struct EndUsersNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> EndUsersNamespace<'a> {
    /// List end users, newest first, one page at a time.
    pub async fn list(
        &self,
        opts: ListEndUsersOptions<'_>,
    ) -> Result<Response<Vec<EndUser>>, KError> {
        let slug = require_project(self.config, "end_users")?;
        if let Some(l) = opts.limit {
            if !(1..=200).contains(&l) {
                return Err(invalid("limit must be a whole number from 1 to 200"));
            }
        }
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(s) = opts.status {
            q.push(("status".into(), s.into()));
        }
        if let Some(s) = opts.q {
            q.push(("q".into(), s.into()));
        }
        if let Some(l) = opts.limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<EndUser>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "end-users"),
            None,
            Some(&q),
        )
        .await
    }

    /// One end user. An unknown id is a 404 `K9O_END_USER_NOT_FOUND`.
    pub async fn get(&self, id: &str) -> Result<Response<EndUser>, KError> {
        let slug = require_project(self.config, "end_users")?;
        check_text(id, "end user id", 64)?;
        request::<EndUser, ()>(
            self.config,
            Method::GET,
            &project_path(slug, &format!("end-users/{}", enc(id))),
            None,
            None,
        )
        .await
    }

    /// The roles end users hold.
    pub fn role_grants(&self) -> EndUserRoleGrantsNamespace<'a> {
        EndUserRoleGrantsNamespace {
            config: self.config,
        }
    }
}

/// Role grants of end users — `client.end_users().role_grants()`.
pub struct EndUserRoleGrantsNamespace<'a> {
    config: &'a HttpClientConfig,
}

impl<'a> EndUserRoleGrantsNamespace<'a> {
    /// The active grants, oldest first. Not paginated. `data` is the grants
    /// array, not the `{ "grants": [...] }` wrapper the gateway sends.
    pub async fn list(
        &self,
        opts: ListRoleGrantsOptions<'_>,
    ) -> Result<Response<Vec<EndUserRoleGrant>>, KError> {
        let slug = require_project(self.config, "end_users")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(id) = opts.end_user_id {
            q.push(("endUserId".into(), id.into()));
        }
        let r = request::<GrantsWrapper, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "end-user-role-grants"),
            None,
            Some(&q),
        )
        .await?;
        Ok(Response {
            data: r.data.grants,
            meta: r.meta,
        })
    }

    /// Give an end user a role. Granting a role the user already holds is a 400
    /// `K9O_INVALID_FIELD_VALUE`; an unknown end user is a 404 `K9O_NOT_FOUND`.
    /// A revoked role can be granted again.
    pub async fn grant(
        &self,
        input: GrantEndUserRoleInput<'_>,
    ) -> Result<Response<EndUserRoleGrant>, KError> {
        let slug = require_project(self.config, "end_users")?;
        check_text(input.end_user_id, "end_user_id", 64)?;
        check_text(input.role, "role", 128)?;
        let r = request::<GrantWrapper, _>(
            self.config,
            Method::POST,
            &project_path(slug, "end-user-role-grants"),
            Some(&input),
            None,
        )
        .await?;
        Ok(Response {
            data: r.data.grant,
            meta: r.meta,
        })
    }

    /// Revoke a grant (it is kept, marked revoked). An unknown or already revoked
    /// id is not an error: the answer is `revoked: false`.
    pub async fn revoke(&self, grant_id: &str) -> Result<Response<RevokeRoleGrantResult>, KError> {
        let slug = require_project(self.config, "end_users")?;
        check_text(grant_id, "grant id", 256)?;
        request::<RevokeRoleGrantResult, ()>(
            self.config,
            Method::DELETE,
            &project_path(slug, &format!("end-user-role-grants/{}", enc(grant_id))),
            None,
            None,
        )
        .await
    }
}
