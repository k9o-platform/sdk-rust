//! Org lifecycle namespace (collection-style) — `client.orgs()`.
//! Distinct from `client.org()` (singular bound-org namespace).

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::url_grammar::{enc, org_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};

/// Org record.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Org {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub status: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Orgs namespace — `client.orgs()`.
pub struct OrgsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> OrgsNamespace<'a> {
    pub async fn list(&self) -> Result<Response<Vec<Org>>, KError> {
        request::<Vec<Org>, ()>(self.config, Method::GET, &org_path("orgs"), None, None).await
    }

    pub async fn get(&self, org_slug: &str) -> Result<Response<Org>, KError> {
        request::<Org, ()>(
            self.config,
            Method::GET,
            &org_path(&format!("orgs/{}", enc(org_slug))),
            None,
            None,
        )
        .await
    }

    pub async fn restore(&self, org_id: &str) -> Result<Response<Org>, KError> {
        request::<Org, ()>(
            self.config,
            Method::POST,
            &org_path(&format!("orgs/{}/restore", enc(org_id))),
            None,
            None,
        )
        .await
    }

    pub async fn current(&self) -> Result<Response<Org>, KError> {
        request::<Org, ()>(self.config, Method::GET, &org_path("org"), None, None).await
    }
}
