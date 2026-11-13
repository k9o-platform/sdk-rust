//! `client.query(sql, params)` — one parameterised, read-only SQL statement against the project's data.
//!
//! `POST /@<slug>/query` with body `{ sql, params? }`; `data` is the array of rows.
//!
//! `params` is either a positional list (bound to `$1, $2, …`; the SQL is sent as written) or a map of named
//! values (`:name` placeholders in the SQL are rewritten to `$n` here and the values become the positional list).
//! Values are bound by the database, never interpolated into the text.
//!
//! The gateway refuses writes (`K9O_RAW_SQL_READ_ONLY`), accepts one statement per call, applies row-level
//! security, allows at most 100000 characters of SQL and 1000 parameters, and needs the project-editor role.
//! A reference to `dataset.<name>` reads an analytics dataset.

use crate::client::Client;
use crate::envelope::Response;
use crate::http::request;
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// The parameters of a query. Values must be a string, number, boolean or null; serialise anything
/// else (for example as JSON text) first.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Params {
    /// No parameters: the body has no `params` key.
    #[default]
    None,
    /// A positional list, bound to `$1, $2, …`.
    Positional(Vec<Value>),
    /// Named values for `:name` placeholders.
    Named(Map<String, Value>),
}

impl From<Vec<Value>> for Params {
    fn from(v: Vec<Value>) -> Self {
        Params::Positional(v)
    }
}

impl From<Map<String, Value>> for Params {
    fn from(m: Map<String, Value>) -> Self {
        Params::Named(m)
    }
}

impl From<()> for Params {
    fn from(_: ()) -> Self {
        Params::None
    }
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn check_value(value: &Value, wher: &str) -> Result<(), KError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
        _ => Err(invalid(format!(
            "{wher} must be a string, number, boolean or null; serialise anything else (for example as JSON text) first"
        ))),
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_char(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

/// Rewrite `:name` placeholders to `$n`. Quoted text, comments, dollar-quoted strings and `::` casts are copied
/// unchanged. Each distinct name gets one number, in order of first appearance.
pub fn translate_named_params(
    sql: &str,
    named: &Map<String, Value>,
) -> Result<(String, Vec<Value>), KError> {
    let b = sql.as_bytes();
    let n = b.len();
    let at = |i: usize| -> u8 { b.get(i).copied().unwrap_or(0) };
    let mut order: Vec<&str> = Vec::new();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let c = b[i];
        let next = at(i + 1);
        if c == b'\''
            || ((c == b'E' || c == b'e') && next == b'\'' && !(i > 0 && is_ident_char(b[i - 1])))
        {
            let escapes = c != b'\'';
            let mut j = if escapes { i + 2 } else { i + 1 };
            while j < n {
                if escapes && b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'\'' {
                    if at(j + 1) == b'\'' {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            let end = (j + 1).min(n);
            out.extend_from_slice(&b[i..end]);
            i = j + 1;
        } else if c == b'"' {
            let mut j = i + 1;
            while j < n {
                if b[j] == b'"' {
                    if at(j + 1) == b'"' {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            let end = (j + 1).min(n);
            out.extend_from_slice(&b[i..end]);
            i = j + 1;
        } else if c == b'-' && next == b'-' {
            let j = b[i..].iter().position(|&x| x == b'\n').map_or(n, |p| i + p);
            out.extend_from_slice(&b[i..j]);
            i = j;
        } else if c == b'/' && next == b'*' {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if b[j] == b'/' && at(j + 1) == b'*' {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && at(j + 1) == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            out.extend_from_slice(&b[i..j]);
            i = j;
        } else if c == b'$' {
            let mut j = i + 1;
            if is_ident_start(at(j)) {
                while j < n && is_ident_char(b[j]) {
                    j += 1;
                }
            }
            if at(j) == b'$' {
                let tag = &b[i..=j];
                let rest = &b[j + 1..];
                let end = match rest.windows(tag.len()).position(|w| w == tag) {
                    Some(p) => j + 1 + p + tag.len(),
                    None => n,
                };
                out.extend_from_slice(&b[i..end]);
                i = end;
            } else {
                out.push(c);
                i += 1;
            }
        } else if c == b':' && next == b':' {
            out.extend_from_slice(b"::");
            i += 2;
        } else if c == b':' && is_ident_start(next) && !(i > 0 && is_ident_char(b[i - 1])) {
            let mut j = i + 2;
            while j < n && is_ident_char(b[j]) {
                j += 1;
            }
            let name = &sql[i + 1..j];
            let idx = match order.iter().position(|x| *x == name) {
                Some(p) => p,
                None => {
                    if !named.contains_key(name) {
                        return Err(
                            invalid(format!("no value for :{name}")).with_context("name", name)
                        );
                    }
                    order.push(name);
                    order.len() - 1
                }
            };
            out.extend_from_slice(format!("${}", idx + 1).as_bytes());
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    let mut values = Vec::with_capacity(order.len());
    for name in order {
        let v = &named[name];
        check_value(v, &format!("the value for :{name}"))?;
        values.push(v.clone());
    }
    // Only whole ASCII-delimited slices of valid UTF-8 were copied, so this cannot fail.
    let text = String::from_utf8(out).map_err(|e| invalid(e.to_string()))?;
    Ok((text, values))
}

/// Build the wire body: `{ sql }` plus `params` when there is at least one value.
fn build_body(sql: &str, params: Params) -> Result<Value, KError> {
    let (text, values) = match params {
        Params::None => (sql.to_string(), Vec::new()),
        Params::Positional(list) => {
            for (i, v) in list.iter().enumerate() {
                check_value(v, &format!("params[{i}]"))?;
            }
            (sql.to_string(), list)
        }
        Params::Named(map) => translate_named_params(sql, &map)?,
    };
    let mut body = Map::new();
    body.insert("sql".into(), Value::String(text));
    if !values.is_empty() {
        body.insert("params".into(), Value::Array(values));
    }
    Ok(Value::Object(body))
}

impl Client {
    /// `POST /@<slug>/query` — run one read-only SQL statement; `data` is the array of rows.
    ///
    /// `params` is `Params::None`, a `Vec<Value>` (positional `$1, $2, …`) or a `serde_json::Map`
    /// (named `:name` placeholders). A placeholder with no key fails with `K9O_INVALID_FIELD_VALUE`
    /// before any request; extra keys are ignored.
    pub async fn query<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl Into<Params>,
    ) -> Result<Response<Vec<T>>, KError> {
        let slug = require_project(&self.config, "query")?;
        let body = build_body(sql, params.into())?;
        let path = project_path(slug, "query");
        request::<Vec<T>, Value>(&self.config, Method::POST, &path, Some(&body), None).await
    }
}
