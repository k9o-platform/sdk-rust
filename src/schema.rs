//! Schema namespace: `client.schema().collections().list(..)`.
//!
//! Wire mapping (project plane): `GET /@<slug>/schema/collections`.
//!
//! The list is cursor-paginated, ordered by collection name: pass
//! `meta.cursor.next` of one page as `cursor` of the next, until it is `None`.

use crate::approvals::check_limit;
use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::{Deserialize, Serialize};

/// One collection (table) of the project's data.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSummary {
    pub name: String,
    /// Live row count, excluding soft-deleted rows. Ignores row-level security.
    pub row_count: u64,
    #[serde(default)]
    pub row_count_is_exact: bool,
    #[serde(default)]
    pub column_count: u64,
    #[serde(default)]
    pub index_count: u64,
    #[serde(default)]
    pub last_migration_slug: Option<String>,
    #[serde(default)]
    pub last_migration_applied_at: Option<String>,
}

/// Options of `schema().collections().list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListCollectionsOptions<'a> {
    /// 1 to 200; the gateway defaults to 50.
    pub limit: Option<u32>,
    /// `meta.cursor.next` of the previous page.
    pub cursor: Option<&'a str>,
}

/// Schema namespace: `client.schema()`.
pub struct SchemaNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> SchemaNamespace<'a> {
    /// The project's collections.
    pub fn collections(&self) -> SchemaCollectionsNamespace<'a> {
        SchemaCollectionsNamespace {
            config: self.config,
        }
    }
}

/// `client.schema().collections()`.
pub struct SchemaCollectionsNamespace<'a> {
    config: &'a HttpClientConfig,
}

impl SchemaCollectionsNamespace<'_> {
    /// List the project's collections by name, one page at a time.
    pub async fn list(
        &self,
        opts: ListCollectionsOptions<'_>,
    ) -> Result<Response<Vec<CollectionSummary>>, KError> {
        check_limit(opts.limit)?;
        let slug = require_project(self.config, "schema")?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(n) = opts.limit {
            q.push(("limit".into(), n.to_string()));
        }
        if let Some(c) = opts.cursor {
            q.push(("cursor".into(), c.into()));
        }
        request::<Vec<CollectionSummary>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "schema/collections"),
            None,
            Some(&q),
        )
        .await
    }
}
