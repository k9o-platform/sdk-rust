//! AI namespace — `client.ai()` (platform AI primitives).
//!
//! `client.ai().embed(..)` is `POST /@<slug>/ai/embeddings` with body
//! `{ model?, input: string | string[] }` and returns
//! `{ data: { embeddings, model, dimensions }, meta }`.
//!
//! `client.ai().generate(prompt, GenerateOptions { .. })` is `POST /@<slug>/ai/generate`, a verb over
//! `compute().llms().invoke` that returns the same `LlmResponse`.
//!
//! Bounds (enforced by the gateway, not here): at most 64 inputs, each
//! non-empty, at most 256 KiB of input per request. The SDK sends the input
//! exactly as given and never puts it in an error it constructs.

use crate::compute::LlmResponse;
use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use reqwest::Method;
use serde::{Deserialize, Serialize};

/// One string or a list of strings; serialised untagged so a single string
/// stays a JSON string on the wire.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum EmbedText {
    One(String),
    Many(Vec<String>),
}

impl From<&str> for EmbedText {
    fn from(s: &str) -> Self {
        EmbedText::One(s.to_string())
    }
}
impl From<String> for EmbedText {
    fn from(s: String) -> Self {
        EmbedText::One(s)
    }
}
impl From<Vec<String>> for EmbedText {
    fn from(v: Vec<String>) -> Self {
        EmbedText::Many(v)
    }
}
impl From<Vec<&str>> for EmbedText {
    fn from(v: Vec<&str>) -> Self {
        EmbedText::Many(v.into_iter().map(String::from).collect())
    }
}

/// Request body for `embed`.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedInput {
    /// Optional; omitted from the body when `None` (server default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input: EmbedText,
}

/// Embedding result: one vector per input string, in input order.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmbedResult {
    pub embeddings: Vec<Vec<f64>>,
    pub model: String,
    pub dimensions: u64,
}

/// Optional fields of `generate`; `None` fields are not sent.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenerateOptions<'a> {
    /// Model id. When `None` the gateway uses the org's default LLM.
    pub model: Option<&'a str>,
    /// 1 to 32000.
    pub max_tokens: Option<u32>,
    /// 0 to 2.
    pub temperature: Option<f64>,
    /// Free-form usage label recorded with the call.
    pub usage_type: Option<&'a str>,
}

/// Wire body of `generate`: prompt first, unset fields omitted.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateBody<'a> {
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_type: Option<&'a str>,
}

/// AI namespace — `client.ai()`.
pub struct AiNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> AiNamespace<'a> {
    /// `POST /@<slug>/ai/embeddings` — embed one or more strings.
    pub async fn embed(&self, input: EmbedInput) -> Result<Response<EmbedResult>, KError> {
        let slug = require_project(self.config, "ai")?;
        let path = project_path(slug, "ai/embeddings");
        request::<EmbedResult, _>(self.config, Method::POST, &path, Some(&input), None).await
    }

    /// `POST /@<slug>/ai/generate` — run a prompt through an LLM. Answers 400 when no model is
    /// given and the org has no default LLM, and 501 `K9O_NOT_IMPLEMENTED` for outside-provider models.
    pub async fn generate(
        &self,
        prompt: &str,
        options: GenerateOptions<'_>,
    ) -> Result<Response<LlmResponse>, KError> {
        let slug = require_project(self.config, "ai")?;
        let path = project_path(slug, "ai/generate");
        let body = GenerateBody {
            prompt,
            model: options.model,
            max_tokens: options.max_tokens,
            temperature: options.temperature,
            usage_type: options.usage_type,
        };
        request::<LlmResponse, _>(self.config, Method::POST, &path, Some(&body), None).await
    }
}
