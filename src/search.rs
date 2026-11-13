//! `client.search(collection, SearchInput { .. })` — full-text search over one text column of a collection.
//!
//! A shortcut for `client.from(collection).select().text_search(column, q, ..)` with an optional page
//! `limit` and `cursor`. The result is the normal list envelope, best matches first.

use crate::client::Client;
use crate::data::{TextSearchOptions, TextSearchType};
use crate::envelope::Response;
use crate::kerror::KError;
use serde::de::DeserializeOwned;

/// Input of `client.search`. `column` and `q` are required.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchInput<'a> {
    /// The text column to search.
    pub column: &'a str,
    /// The search text.
    pub q: &'a str,
    /// A text-search configuration name; default `english`.
    pub language: Option<&'a str>,
    /// `Plain` (default), `Phrase` or `Websearch`.
    pub search_type: Option<TextSearchType>,
    /// Page size.
    pub limit: Option<u64>,
    /// Opaque cursor from a previous response's `meta.cursor.next`.
    pub cursor: Option<&'a str>,
}

impl Client {
    /// Full-text search over one text column, best matches first. Invalid input fails with
    /// `K9O_INVALID_FIELD_VALUE` before any request.
    pub async fn search<T: DeserializeOwned>(
        &self,
        collection: &str,
        input: SearchInput<'_>,
    ) -> Result<Response<Vec<T>>, KError> {
        let mut b = self.from(collection).select().text_search(
            input.column,
            input.q,
            TextSearchOptions {
                language: input.language,
                search_type: input.search_type.unwrap_or_default(),
            },
        );
        if let Some(n) = input.limit {
            b = b.limit(n);
        }
        if let Some(c) = input.cursor {
            b = b.cursor(c);
        }
        b.execute::<T>().await
    }
}
