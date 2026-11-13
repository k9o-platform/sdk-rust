//! Compute namespace — `client.compute()` (Compute LLMs
//! registry).
//!
//! Rust idiomatic: `Result<Response<T>, KError>`;
//! `invoke_stream` returns an `SseStream<LlmStreamChunk>` async-iterated
//! via `next().await`.

use crate::envelope::Response;
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::{open_sse_stream, require_project, SseStream};
use crate::url_grammar::{enc, project_path};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Model metadata with per-model cost.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    pub id: String,
    pub provider: String,
    pub provider_class: String,
    pub modality: String,
    /// Lowest project tier that can use the model.
    pub min_tier: String,
    /// False when the model needs a tenant key (see `reason`).
    pub invocable: bool,
    /// Why the model is not invocable, e.g. `tenant_key_only`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Price per million input tokens; `None` when the model has no price.
    #[serde(default)]
    pub price_in: Option<f64>,
    /// Price per million output tokens; `None` when the model has no price.
    #[serde(default)]
    pub price_out: Option<f64>,
    /// Unit of `price_in` and `price_out`, e.g. `usd_per_million_tokens`.
    #[serde(default)]
    pub price_unit: Option<String>,
}

/// Token counts the model server reported for a call.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Synchronous inference result of `/invoke`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmResponse {
    pub model_id: String,
    /// The generated text.
    pub content: String,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub usage: LlmUsage,
    #[serde(default)]
    pub usage_type: String,
}

/// One chunk of the `/invoke-stream` response: the model server's
/// OpenAI-style chat-completion chunk, passed through unchanged. Read
/// incremental text from `choices[0].delta.content`; the final chunk may
/// carry `usage`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LlmStreamChunk {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<LlmStreamChoice>,
    #[serde(default)]
    pub usage: Option<Value>,
}

/// One choice of a stream chunk.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LlmStreamChoice {
    #[serde(default)]
    pub index: u64,
    #[serde(default)]
    pub delta: LlmStreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental part of a stream choice.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LlmStreamDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// Inference parameters the route reads; sent flat in the request body.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmInvokeParams<'a> {
    /// 1 to 32000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 0 to 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Free-form usage label recorded with the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_type: Option<&'a str>,
}

/// Input body for invoke / invoke_stream. The wire body is
/// `{prompt, maxTokens?, temperature?, usageType?}`.
#[derive(Debug, Serialize)]
pub struct LlmInvokeInput<'a> {
    pub prompt: &'a str,
    #[serde(flatten)]
    pub params: LlmInvokeParams<'a>,
}

/// Curated closed model enum (extensible v2.x).
pub const LLM_MODEL_IDS: [&str; 9] = [
    "mistral-7b",
    "mistral-8x7b",
    "llama-3-8b",
    "llama-3-70b",
    "claude-3-5-sonnet",
    "gpt-4o",
    "gpt-4o-mini",
    "text-embedding-3-small",
    "text-embedding-3-large",
];

/// LLMs namespace — `client.compute().llms()`
pub struct LlmsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> LlmsNamespace<'a> {
    /// List the models available to the project tier: `GET /@<slug>/compute/llms`.
    pub async fn list(&self) -> Result<Response<Vec<LlmModel>>, KError> {
        let slug = require_project(self.config, "compute.llms")?;
        request::<Vec<LlmModel>, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "compute/llms"),
            None,
            None,
        )
        .await
    }

    pub async fn read(&self, model_id: &str) -> Result<Response<LlmModel>, KError> {
        let slug = require_project(self.config, "compute.llms")?;
        let path = project_path(slug, &format!("compute/llms/{}", enc(model_id)));
        request::<LlmModel, ()>(self.config, Method::GET, &path, None, None).await
    }

    /// Synchronous inference (PROJECT_AUTHOR/ADMIN).
    pub async fn invoke(
        &self,
        model_id: &str,
        input: LlmInvokeInput<'_>,
    ) -> Result<Response<LlmResponse>, KError> {
        let slug = require_project(self.config, "compute.llms")?;
        let path = project_path(slug, &format!("compute/llms/{}/invoke", enc(model_id)));
        request::<LlmResponse, _>(self.config, Method::POST, &path, Some(&input), None).await
    }

    /// Streaming inference (SSE per-token) Iterate the
    /// returned stream with `next().await`.
    pub async fn invoke_stream(
        &self,
        model_id: &str,
        input: LlmInvokeInput<'_>,
    ) -> Result<SseStream<LlmStreamChunk>, KError> {
        let slug = require_project(self.config, "compute.llms")?;
        let path = project_path(
            slug,
            &format!("compute/llms/{}/invoke-stream", enc(model_id)),
        );
        open_sse_stream::<LlmStreamChunk, _>(self.config, Method::POST, &path, Some(&input), None)
            .await
    }
}

/// Compute namespace grouping the compute surfaces.
pub struct ComputeNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> ComputeNamespace<'a> {
    pub fn llms(&self) -> LlmsNamespace<'a> {
        LlmsNamespace {
            config: self.config,
        }
    }
}
