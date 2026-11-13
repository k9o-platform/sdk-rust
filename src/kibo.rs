//! `client.kibo().vault()` — the Kibo vault surface, ported from the
//! TypeScript SDK (`packages/kibo/src/vault.ts` / `types.ts`).
//!
//! Structural note: TS exposes `client.kibo.vault.{zkDeks, secrets}`. This
//! port implements ONLY `secrets`; porting `zk_deks` is a separate scoping
//! call left for a follow-up rather than assumed here.
//!
//! The secrets family: list / get / create / reveal_challenge / reveal /
//! update / rotate / `r#move` / delete. The `region` argument of every
//! operation on a NAMED secret is optional: the gateway sets the residency
//! region of every vault call itself and discards a caller-supplied `region`
//! query parameter. Pass `""` to omit it; a non-empty region is still sent
//! (older gateways required it).
//!
//! Response shapes for `update`/`rotate`/`move` are typed to what the
//! gateway ACTUALLY returns, not the OpenAPI's documented (and inaccurate for
//! these three) `SecretSummary` — see the notes on
//! [`UpdateSecretResult`]/[`RotateSecretResult`]/[`MoveSecretResult`] below.

use crate::envelope::Response;
use crate::http::{request, request_with_headers, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::{enc, project_path};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Query of a named-secret call: `op` (if any) plus `region` (only when non-empty).
fn region_query(op: Option<&str>, region: &str) -> Vec<(String, String)> {
    let mut q: Vec<(String, String)> = Vec::new();
    if let Some(op) = op {
        q.push(("op".into(), op.into()));
    }
    if !region.is_empty() {
        q.push(("region".into(), region.into()));
    }
    q
}

/// The `:name` segment is hierarchical (e.g. `db/pw`) and has its `/` escaped
/// so it stays a single path segment (`db%2Fpw` on the wire).
fn secret_path(project_slug: &str, name: &str) -> String {
    project_path(project_slug, &format!("vault/secrets/{}", enc(name)))
}

/// UTF-8-safe base64 encode — the vault-secrets wire `value` field is base64
/// (live-verified; the OpenAPI "plaintext to seal" description does not mean
/// raw-on-the-wire — a raw non-base64 `value` is rejected as unparseable).
fn encode_base64_utf8(plaintext: &str) -> String {
    BASE64.encode(plaintext.as_bytes())
}

/// Inverse of [`encode_base64_utf8`] — decodes a revealed wire `value` back
/// to plaintext.
fn decode_base64_utf8(b64: &str) -> Result<String, KError> {
    let bytes = BASE64.decode(b64).map_err(|e| {
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!("reveal() value is not valid base64: {}", e),
        )
    })?;
    String::from_utf8(bytes).map_err(|e| {
        KError::new(
            "K9O_INTERNAL_ERROR",
            format!(
                "reveal() value is not valid UTF-8 after base64 decode: {}",
                e
            ),
        )
    })
}

/// Secret visibility — write ops (create/update) stay owner-only
/// regardless of this tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SecretVisibility {
    Private,
    Shared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principals: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        roles: Option<Vec<String>>,
    },
}

/// A secret WITHOUT its sealed ciphertext — the create/list/read metadata
/// shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretSummary {
    /// `sec_<base62>`.
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub visibility: SecretVisibility,
    pub version: u64,
    /// The wrapping-key id at this version.
    pub kid: String,
    pub created_at: String,
    pub created_by: String,
    pub updated_at: String,
    pub updated_by: String,
    pub rotated_at: Option<String>,
    pub retired_at: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// `secrets().list()` input.
#[derive(Debug, Default, Clone)]
pub struct ListSecretsInput<'a> {
    /// Filter to a path prefix.
    pub prefix: Option<&'a str>,
    pub cursor: Option<&'a str>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListSecretsResult {
    pub secrets: Vec<SecretSummary>,
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// Input to create a vault secret — `POST /@<projectSlug>/vault/secrets/<name>`.
#[derive(Debug, Clone)]
pub struct CreateSecretInput<'a> {
    /// Hierarchical path the secret is addressed under, e.g. `byos/s3-prod`
    /// (also the `:name` URL segment).
    pub name: &'a str,
    /// The plaintext to seal, as a regular (non-base64) string — `create`
    /// base64-encodes it for the wire itself (live-verified: the route's
    /// OpenAPI description says "the plaintext to seal" but a raw
    /// non-base64 `value` is rejected as unparseable).
    pub value: &'a str,
    /// Optional region (`""` omits it): the gateway sets the region itself and
    /// discards a caller-supplied one; a non-empty value is still sent as
    /// `?region=` for older gateways.
    pub region: &'a str,
    pub visibility: Option<SecretVisibility>,
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct CreateSecretBody<'a> {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<&'a SecretVisibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a HashMap<String, String>>,
}

/// Releasing-node observed attestation tier (K4) — present on every
/// reveal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationTier {
    /// `attestation:destination:region` trust-tier URN.
    pub urn: String,
    /// Node provenance: `platform` (Kibo-operated) or `client` (Private
    /// Perimeter).
    pub operator: String,
    /// Whether node attestation was verified against the vendor PKI on the
    /// URN's attestation segment.
    pub verified: bool,
}

/// `reveal_challenge()` result — a single-use challenge to present on the
/// immediately-following `reveal()` call.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevealChallenge {
    pub challenge: String,
    pub expires_at: String,
}

/// Wire shape of `reveal()`'s response — `value` is base64 on the wire;
/// [`SecretsNamespace::reveal`] decodes it into the public
/// [`RevealSecretResult`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevealSecretResultWire {
    id: String,
    name: String,
    version: u64,
    value: String,
    revealed_at: String,
    attestation_tier: AttestationTier,
}

/// `reveal()` result. `value` is the decoded plaintext (base64-decoded from
/// the wire value, the inverse of `create`'s encode) — never the raw
/// base64.
#[derive(Debug, Clone)]
pub struct RevealSecretResult {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub value: String,
    pub revealed_at: String,
    pub attestation_tier: AttestationTier,
}

/// `update()` input.
#[derive(Debug, Clone)]
pub struct UpdateSecretInput<'a> {
    /// Optional region (`""` omits it), as on [`CreateSecretInput`].
    pub region: &'a str,
    /// New plaintext — bumps `version`. Base64-encoded for the wire.
    pub value: &'a str,
    pub metadata: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct UpdateSecretBody<'a> {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a HashMap<String, String>>,
}

/// `update()` result — live-verified NARROWER than the documented
/// `SecretSummary`: the gateway returns only `{id, name, version}` on a
/// real PUT, not the full summary the OpenAPI schema claims. Typed to what
/// is actually observed rather than the aspirational doc shape; call
/// `get()` afterward if the full summary is needed.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSecretResult {
    pub id: String,
    pub name: String,
    pub version: u64,
}

/// `rotate()` result — live-verified shape, not documented anywhere in the
/// OpenAPI schema (the route only declares a union this matches neither).
/// `ledger_sequence_number` is undocumented; kept as-observed.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RotateSecretResult {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub rotated_at: String,
    pub ledger_sequence_number: u64,
}

/// `move()` result — live-verified shape, also not the documented
/// `SecretSummary`: no `version`, and an undocumented `event` field
/// (`"SECRET_PATH_CHANGED"` observed; typed as `String` since only one
/// sample was seen and a closed enum can't be asserted from one
/// observation).
#[derive(Debug, Clone, Deserialize)]
pub struct MoveSecretResult {
    pub id: String,
    pub name: String,
    pub event: String,
}

#[derive(Serialize)]
struct MoveSecretBody<'a> {
    #[serde(rename = "newPath")]
    new_path: &'a str,
}

/// Bound on the pages `get` walks while looking for an exact name.
const MAX_GET_PAGES: usize = 20;

const SURFACE: &str = "kibo.vault.secrets";

/// Typed secrets surface — `client.kibo().vault().secrets()`.
pub struct SecretsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> SecretsNamespace<'a> {
    /// List secrets (metadata only — never lists ciphertext).
    /// `GET /@<projectSlug>/vault/secrets`.
    pub async fn list(
        &self,
        input: ListSecretsInput<'_>,
    ) -> Result<Response<ListSecretsResult>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let mut q: Vec<(String, String)> = Vec::new();
        if let Some(p) = input.prefix {
            q.push(("prefix".into(), p.into()));
        }
        if let Some(c) = input.cursor {
            q.push(("cursor".into(), c.into()));
        }
        if let Some(l) = input.limit {
            q.push(("limit".into(), l.to_string()));
        }
        request::<ListSecretsResult, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "vault/secrets"),
            None,
            Some(&q),
        )
        .await
    }

    /// Read a secret's metadata (never its value — use `reveal_challenge` +
    /// `reveal` for that). The vault answers `GET .../vault/secrets/<name>`
    /// only as a reveal (401 `K9O_REVEAL_CHALLENGE_REQUIRED` without a
    /// challenge), so `get` lists by the exact name as prefix
    /// (`GET .../vault/secrets?prefix=<name>&limit=100`) and picks the exact
    /// match. Returns `K9O_NOT_FOUND` (404) when no secret has that name.
    /// `region` is accepted for symmetry and not sent.
    pub async fn get(&self, name: &str, _region: &str) -> Result<Response<SecretSummary>, KError> {
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_GET_PAGES {
            let page = self
                .list(ListSecretsInput {
                    prefix: Some(name),
                    cursor: cursor.as_deref(),
                    limit: Some(100),
                })
                .await?;
            let Response { data, meta } = page;
            if let Some(hit) = data.secrets.into_iter().find(|s| s.name == name) {
                return Ok(Response { data: hit, meta });
            }
            match data.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        let mut err = KError::new(
            "K9O_NOT_FOUND",
            format!("No secret named '{}' in this project.", name),
        )
        .with_context("name", name);
        err.status = Some(404);
        Err(err)
    }

    /// Create a secret under the caller's own auth.
    /// `POST /@<projectSlug>/vault/secrets/<name>` → the `sec_<base62>` summary.
    pub async fn create(
        &self,
        input: CreateSecretInput<'_>,
    ) -> Result<Response<SecretSummary>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, input.name);
        let q = region_query(None, input.region);
        let body = CreateSecretBody {
            value: encode_base64_utf8(input.value),
            visibility: input.visibility.as_ref(),
            metadata: input.metadata.as_ref(),
        };
        request::<SecretSummary, _>(self.config, Method::POST, &path, Some(&body), Some(&q)).await
    }

    /// Mint a single-use reveal challenge — present it as
    /// `X-K9O-Reveal-Challenge` on the immediately-following `reveal()`
    /// call. `POST .../<name>?op=reveal-challenge&region=<region>`.
    pub async fn reveal_challenge(
        &self,
        name: &str,
        region: &str,
    ) -> Result<Response<RevealChallenge>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(Some("reveal-challenge"), region);
        request::<RevealChallenge, ()>(self.config, Method::POST, &path, None, Some(&q)).await
    }

    /// Reveal a secret's plaintext value using a single-use challenge from
    /// `reveal_challenge()`. `GET .../<name>?region=<region>` with
    /// `X-K9O-Reveal-Challenge`. Decodes the wire's base64 `value` back to
    /// plaintext (live-verified: the reveal response's `value` is base64,
    /// symmetric with `create`'s encode).
    pub async fn reveal(
        &self,
        name: &str,
        region: &str,
        challenge: &str,
    ) -> Result<Response<RevealSecretResult>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(None, region);
        let headers = [("X-K9O-Reveal-Challenge".to_string(), challenge.to_string())];
        let env = request_with_headers::<RevealSecretResultWire, ()>(
            self.config,
            Method::GET,
            &path,
            None,
            Some(&q),
            Some(&headers),
        )
        .await?;
        let plaintext = decode_base64_utf8(&env.data.value)?;
        Ok(Response {
            data: RevealSecretResult {
                id: env.data.id,
                name: env.data.name,
                version: env.data.version,
                value: plaintext,
                revealed_at: env.data.revealed_at,
                attestation_tier: env.data.attestation_tier,
            },
            meta: env.meta,
        })
    }

    /// Update a secret's value (bumps version) — owner-only.
    /// `PUT .../<name>?region=<region>`. Returns only `{id, name, version}`
    /// live (see [`UpdateSecretResult`]), not the full summary the OpenAPI
    /// schema documents — call `get()` after if you need the rest.
    pub async fn update(
        &self,
        name: &str,
        input: UpdateSecretInput<'_>,
    ) -> Result<Response<UpdateSecretResult>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(None, input.region);
        let body = UpdateSecretBody {
            value: encode_base64_utf8(input.value),
            metadata: input.metadata.as_ref(),
        };
        request::<UpdateSecretResult, _>(self.config, Method::PUT, &path, Some(&body), Some(&q))
            .await
    }

    /// Rotate a secret's wrapping key (re-wraps under a fresh `kid`;
    /// content unchanged) — owner-only. `POST .../<name>?op=rotate&region=<region>`.
    pub async fn rotate(
        &self,
        name: &str,
        region: &str,
    ) -> Result<Response<RotateSecretResult>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(Some("rotate"), region);
        request::<RotateSecretResult, ()>(self.config, Method::POST, &path, None, Some(&q)).await
    }

    /// Move (rename) a secret's hierarchical path — owner-only.
    /// `POST .../<name>?op=move&region=<region>`. Named `r#move` — `move`
    /// is a Rust keyword.
    pub async fn r#move(
        &self,
        name: &str,
        region: &str,
        new_path: &str,
    ) -> Result<Response<MoveSecretResult>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(Some("move"), region);
        let body = MoveSecretBody { new_path };
        request::<MoveSecretResult, _>(self.config, Method::POST, &path, Some(&body), Some(&q))
            .await
    }

    /// Retire a secret — owner-only. `DELETE .../<name>?region=<region>` →
    /// `204 No Content`.
    pub async fn delete(&self, name: &str, region: &str) -> Result<Response<Value>, KError> {
        let slug = require_project(self.config, SURFACE)?;
        let path = secret_path(slug, name);
        let q = region_query(None, region);
        request::<Value, ()>(self.config, Method::DELETE, &path, None, Some(&q)).await
    }
}

/// `client.kibo().vault()` — both `vault.zk_deks` (not yet ported to Rust)
/// and `vault.secrets` require a project context (member-plane).
pub struct VaultNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> VaultNamespace<'a> {
    /// Typed secrets surface.
    pub fn secrets(&self) -> SecretsNamespace<'a> {
        SecretsNamespace {
            config: self.config,
        }
    }
}

/// `client.kibo()` — the Kibo (BYOK/HYOK vault) surface root.
pub struct KiboNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> KiboNamespace<'a> {
    pub fn vault(&self) -> VaultNamespace<'a> {
        VaultNamespace {
            config: self.config,
        }
    }
}
