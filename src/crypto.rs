//! Server-side encryption: `client.crypto()`.
//!
//! Wire mapping (project plane):
//!   - `crypto().encrypt(crypt, items)` -> `POST /@<slug>/crypto/encrypt`
//!   - `crypto().decrypt(crypt, items)` -> `POST /@<slug>/crypto/decrypt`
//!   - `crypto().rewrap(crypt, items)`  -> `POST /@<slug>/crypto/rewrap`
//!
//! You never name a key. A ciphertext is one opaque container (base64); the SDK never parses it.
//! Nothing is retried after a failure.
//!
//! Routing: `encrypt` reads the project's custody mode (`GET /@<slug>/crypto/mode`) on every call and
//! sends plaintext to the platform only when the platform holds the key. When the key is in your own
//! KMS or HSM (BYOK or HYOK) the call goes to the registered [`ClientCore`]; with none registered it
//! fails with `K9O_ZK_CLIENT_CORE_UNAVAILABLE` and nothing is sent. `decrypt` and `rewrap` try the
//! platform first and move to the client core when the platform answers 409
//! `K9O_ZK_STORE_FLOW_REQUIRED` with `context.route == "client"`.

use crate::envelope::{Meta, Response};
use crate::http::{request, HttpClientConfig};
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::{Arc, RwLock};

/// 404 on encrypt: the crypt name is not registered. Passes through unchanged.
pub const CODE_CRYPT_NOT_FOUND: &str = "K9O_CRYPT_NOT_FOUND";
/// 409 on encrypt: the crypt name is retired. Decrypt and rewrap are not refused for it.
pub const CODE_CRYPT_RETIRED: &str = "K9O_CRYPT_RETIRED";
/// 409: the call needs the client-side flow (the key is in your own KMS or HSM).
pub const CODE_ZK_STORE_FLOW_REQUIRED: &str = "K9O_ZK_STORE_FLOW_REQUIRED";
/// 501 (raised by the SDK): the call needs the Kibo client core and none is registered.
pub const CODE_ZK_CLIENT_CORE_UNAVAILABLE: &str = "K9O_ZK_CLIENT_CORE_UNAVAILABLE";
/// Raised by the SDK when the custody check gave no usable answer; nothing was sent.
/// Raised by the SDK before the call reaches the client core: `expected_key_refs` is set and the
/// core does not declare that it enforces it. No HTTP status.
pub const CODE_KEY_PIN_UNENFORCED: &str = "K9O_CRYPTO_KEY_PIN_UNENFORCED";
pub const CODE_CUSTODY_MODE_UNKNOWN: &str = "K9O_CRYPTO_CUSTODY_UNKNOWN";

/// Most items in one call.
pub const MAX_ITEMS: usize = 100;
/// Largest plaintext of one item, in bytes (64 KiB).
pub const MAX_ITEM_BYTES: usize = 64 * 1024;
/// Largest total plaintext of one call, in bytes (1 MiB).
pub const MAX_CALL_BYTES: usize = 1024 * 1024;
/// Longest `context`, in bytes.
pub const MAX_CONTEXT_BYTES: usize = 256;

/// One item to encrypt: bytes, and an optional `context` that must be given again on decrypt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainItem {
    pub plaintext: Vec<u8>,
    pub context: Option<String>,
}

impl PlainItem {
    pub fn new(plaintext: impl Into<Vec<u8>>) -> Self {
        Self {
            plaintext: plaintext.into(),
            context: None,
        }
    }

    /// A UTF-8 string as the plaintext.
    pub fn text(plaintext: &str) -> Self {
        Self::new(plaintext.as_bytes().to_vec())
    }

    /// Bind the item to a `context` (at most 256 bytes).
    pub fn context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// One item to decrypt or rewrap: the base64 `ciphertext` exactly as `encrypt` returned it, and the
/// `context` it was encrypted with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedItem {
    pub ciphertext: String,
    pub context: Option<String>,
}

impl SealedItem {
    pub fn new(ciphertext: impl Into<String>) -> Self {
        Self {
            ciphertext: ciphertext.into(),
            context: None,
        }
    }

    pub fn context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
}

/// The result of one item of a batch: a `ciphertext` (encrypt, rewrap), a `plaintext` (decrypt) or a
/// `code` and `message` (that item failed). The item shape follows the gateway; fields the SDK does
/// not know are kept in `extra`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CryptoItemResult {
    #[serde(default)]
    pub ciphertext: Option<String>,
    /// The plaintext as the base64 the wire carries; see [`Self::plaintext_bytes`].
    #[serde(default)]
    pub plaintext: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CryptoItemResult {
    /// True when this item failed (the gateway sent a `code`).
    pub fn is_error(&self) -> bool {
        self.code.is_some()
    }

    /// The decrypted bytes, or `None` when this item has no plaintext (or it is not base64).
    pub fn plaintext_bytes(&self) -> Option<Vec<u8>> {
        let p = self.plaintext.as_deref()?;
        STANDARD
            .decode(p)
            .or_else(|_| URL_SAFE.decode(p))
            .or_else(|_| URL_SAFE_NO_PAD.decode(p))
            .ok()
    }

    /// The decrypted bytes as UTF-8 text, or `None` when absent or not valid UTF-8.
    pub fn plaintext_text(&self) -> Option<String> {
        String::from_utf8(self.plaintext_bytes()?).ok()
    }

    /// The failure of this item as a [`KError`], or `None` when it succeeded.
    pub fn error(&self) -> Option<KError> {
        let code = self.code.as_ref()?;
        Some(KError::new(
            code.clone(),
            self.message.clone().unwrap_or_default(),
        ))
    }
}

/// The answer of `encrypt`, `decrypt` and `rewrap`: one result per input item, in the same order.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CryptoBatch {
    #[serde(default)]
    pub items: Vec<CryptoItemResult>,
}

fn invalid(m: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", format!("crypto: {}", m.into()))
}

fn too_large(m: impl Into<String>) -> KError {
    KError::new("K9O_PAYLOAD_TOO_LARGE", format!("crypto: {}", m.into()))
}

fn check_crypt(crypt: &str) -> Result<(), KError> {
    let ok = (1..=64).contains(&crypt.len())
        && crypt != "actions"
        && crypt
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "crypt must be 1 to 64 characters of a-z, 0-9 and -, and not \"actions\"",
        ))
    }
}

fn check_count(n: usize) -> Result<(), KError> {
    if (1..=MAX_ITEMS).contains(&n) {
        Ok(())
    } else {
        Err(invalid("items must hold 1 to 100 entries"))
    }
}

fn check_context(c: &Option<String>) -> Result<(), KError> {
    match c {
        Some(c) if c.len() > MAX_CONTEXT_BYTES => Err(invalid("context is at most 256 bytes")),
        _ => Ok(()),
    }
}

/// One crypt in the project's registry (`crypts().list`, `create`, `retire`).
///
/// A registered name is a label `encrypt` accepts; it is not an isolation or enforcement boundary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Crypt {
    pub name: String,
    /// `active` or `retired`.
    pub status: CryptStatus,
    /// ISO 8601 date-time string.
    pub created_at: String,
    /// ISO 8601 date-time string; `None` while the crypt is active.
    pub retired_at: Option<String>,
}

/// The status of a [`Crypt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CryptStatus {
    Active,
    Retired,
}

/// The answer of `crypts().list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CryptList {
    pub crypts: Vec<Crypt>,
}

fn crypt_malformed(op: &str, why: &str) -> KError {
    KError::new(
        "K9O_INTERNAL_ERROR",
        format!("crypto.crypts.{op}: the gateway answered {why}"),
    )
}

fn crypt_of(d: &Value, op: &str) -> Result<Crypt, KError> {
    let o = d
        .as_object()
        .ok_or_else(|| crypt_malformed(op, "without a crypt"))?;
    let text = |k: &str| o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let name = text("name").ok_or_else(|| crypt_malformed(op, "without a name"))?;
    let status = match o.get("status").and_then(Value::as_str) {
        Some("active") => CryptStatus::Active,
        Some("retired") => CryptStatus::Retired,
        _ => return Err(crypt_malformed(op, "with an unknown status")),
    };
    let created_at =
        text("createdAt").ok_or_else(|| crypt_malformed(op, "without createdAt"))?;
    let retired_at = match o.get("retiredAt") {
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => {
            return Err(crypt_malformed(
                op,
                "with a retiredAt that is neither a date nor null",
            ))
        }
    };
    Ok(Crypt {
        name: name.to_string(),
        status,
        created_at: created_at.to_string(),
        retired_at,
    })
}

fn check_crypt_name(name: &str, op: &str) -> Result<(), KError> {
    check_crypt(name).map_err(|_| {
        KError::new(
            "K9O_INVALID_FIELD_VALUE",
            format!("crypto.crypts.{op}: name must be 1 to 64 characters of a-z, 0-9 and -, and not \"actions\""),
        )
        .with_context("field", "name")
    })
}

/// The project's custody state, from `GET /@<slug>/crypto/mode`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CryptoMode {
    /// `PLATFORM_MANAGED`, `CONFIDENTIAL_WORKLOAD`, `SKR`, `BYOK` or `HYOK`.
    #[serde(default)]
    pub custody_mode: String,
    /// Opaque; changes with every custody transition.
    #[serde(default)]
    pub epoch: String,
    /// The route the gateway names for `encrypt`: `"server"` or `"client"`. `None` when the answer
    /// has no `route`; any other value (a JSON null included) is kept as sent so `encrypt` can refuse it.
    #[serde(default, deserialize_with = "keep_route")]
    pub route: Option<Value>,
}

fn keep_route<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// A short-lived token for your own cloud KMS or HSM, from `POST /@<slug>/crypto/federated-token`.
///
/// `token` is a credential: the `Debug` output redacts it. Keep it out of logs and error messages.
/// `provider` and `project_id` are present only when the gateway sends them.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CryptoFederatedToken {
    /// A signed RS256 JWT, valid for at most 15 minutes.
    #[serde(default)]
    pub token: String,
    /// When the token stops being valid (date-time string).
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub audience: String,
    /// The id of the signing key.
    #[serde(default)]
    pub kid: String,
    /// The cloud the gateway says the token is for (`aws`, `gcp`, `azure` or `vault`), when it
    /// sends one. Optional, and never a source of configuration: a client core only checks it
    /// against its own.
    #[serde(default)]
    pub provider: Option<String>,
    /// The project id the gateway says the token is for (`projectId` on the wire), when it sends
    /// one. Optional, and only checked against configuration.
    #[serde(default)]
    pub project_id: Option<String>,
}

impl std::fmt::Debug for CryptoFederatedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CryptoFederatedToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("issuer", &self.issuer)
            .field("subject", &self.subject)
            .field("audience", &self.audience)
            .field("kid", &self.kid)
            .field("provider", &self.provider)
            .field("project_id", &self.project_id)
            .finish()
    }
}

/// What the router passes to the client core on every call.
///
/// `expected_key_refs` is the list you set in [`crate::ClientOpts`]. When it is present the core
/// must refuse a key whose reference (version ignored, see [`key_ref_matches`]) is not in it, before
/// it uses any token or makes any network call. It is `None` when you did not set it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreOptions<'a> {
    pub expected_key_refs: Option<&'a [String]>,
}

/// Check `expected_key_refs` from the client options. It must be a non-empty list of non-blank
/// entries without whitespace, and none may contain `*`, since a wildcard pins nothing.
pub(crate) fn check_expected_key_refs(
    refs: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, KError> {
    let Some(refs) = refs else {
        return Ok(None);
    };
    let bad = |m: &str| {
        KError::new("K9O_INVALID_FIELD_VALUE", format!("expectedKeyRefs: {m}"))
            .with_context("field", "expectedKeyRefs")
    };
    if refs.is_empty() {
        return Err(bad(
            "pass a non-empty list of key identifiers, or leave it out",
        ));
    }
    for r in &refs {
        if r.trim().is_empty() || r.chars().any(char::is_whitespace) {
            return Err(bad("each entry must be a key identifier without spaces"));
        }
        if r.contains('*') {
            return Err(bad("an entry cannot contain \"*\"; name the exact key"));
        }
    }
    Ok(Some(refs))
}

/// The provider whose key reference rules apply (see [`key_ref_matches`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefProvider {
    Aws,
    Gcp,
    Azure,
    Vault,
}

fn key_identity(r: &str, provider: KeyRefProvider) -> String {
    match provider {
        KeyRefProvider::Aws => r.to_string(),
        KeyRefProvider::Gcp => r
            .split("/cryptoKeyVersions/")
            .next()
            .unwrap_or(r)
            .to_string(),
        KeyRefProvider::Azure => r
            .split('/')
            .take(5)
            .collect::<Vec<_>>()
            .join("/")
            .to_ascii_lowercase(),
        KeyRefProvider::Vault => {
            let (path, key) = match r.rfind('/') {
                Some(i) => (&r[..i], &r[i + 1..]),
                None => ("", r),
            };
            let key = key.split(':').next().unwrap_or(key);
            format!("{path}/{key}")
        }
    }
}

/// Whether `key_ref` is one of `expected`, ignoring the key version, by the rule of the named
/// provider: `Aws` compares the whole reference; `Gcp` drops `/cryptoKeyVersions/<n>` and what
/// follows; `Azure` compares the first five `/`-separated parts (`https://<host>/keys/<key>`) in
/// lower case, so a version and the case of the host and key name are ignored; `Vault` compares
/// `<path>/<key>` without a `:<version>` suffix. The provider is required and never guessed from
/// the reference: a Vault reference can look like a URL. For authors of a client core; the SDK does
/// not call it.
pub fn key_ref_matches(provider: KeyRefProvider, key_ref: &str, expected: &[String]) -> bool {
    let k = key_identity(key_ref, provider);
    expected.iter().any(|e| key_identity(e, provider) == k)
}

/// The client-side crypto core: the hook the language's Kibo binding fills. It runs encrypt, decrypt
/// and rewrap against the key in your own KMS or HSM, so plaintext and key never reach the platform.
/// Register one with [`CryptoNamespace::use_client_core`]. The contract carries no epoch. Every call
/// gets the [`CoreOptions`] built from the client options.
#[async_trait]
pub trait ClientCore: Send + Sync {
    /// Return `true` only when this core refuses a key that is not in
    /// [`CoreOptions::expected_key_refs`], before it uses any token or makes any network call. When
    /// you set `expected_key_refs` on the client and the core does not declare this, the call is
    /// refused with `K9O_CRYPTO_KEY_PIN_UNENFORCED` before it reaches the core, because a pin the
    /// core ignores would look like a pin. Defaults to `false`.
    fn enforces_expected_key_refs(&self) -> bool {
        false
    }
    async fn encrypt(
        &self,
        crypt: &str,
        items: &[PlainItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError>;
    async fn decrypt(
        &self,
        crypt: &str,
        items: &[SealedItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError>;
    async fn rewrap(
        &self,
        crypt: &str,
        items: &[SealedItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError>;
}

/// The registered client core, shared by every handle of one client.
#[derive(Default)]
pub(crate) struct CryptoState {
    core: RwLock<Option<Arc<dyn ClientCore>>>,
}

impl std::fmt::Debug for CryptoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self.core.read().map(|c| c.is_some()).unwrap_or(false);
        f.debug_struct("CryptoState")
            .field("client_core", &set)
            .finish()
    }
}

fn is_server_mode(m: &str) -> bool {
    matches!(m, "PLATFORM_MANAGED" | "CONFIDENTIAL_WORKLOAD" | "SKR")
}

fn is_client_mode(m: &str) -> bool {
    matches!(m, "BYOK" | "HYOK")
}

fn core_unavailable(mode: &str) -> KError {
    let mut e = KError::new(
        CODE_ZK_CLIENT_CORE_UNAVAILABLE,
        "crypto: this project's key is held in your own KMS; install and register the Kibo client core for this language, or call kibo.encrypt/decrypt directly",
    )
    .with_context("custodyMode", mode)
    .with_context("requirement", "kibo-client-core");
    e.status = Some(501);
    e
}

fn key_pin_unenforced(mode: &str) -> KError {
    KError::new(
        CODE_KEY_PIN_UNENFORCED,
        "expectedKeyRefs is set but the client core does not declare that it enforces it; nothing was sent",
    )
    .with_context("custodyMode", mode)
    .with_context("requirement", "client-core-enforces-expected-key-refs")
    .with_recommendation(
        "Use a client core that enforces the pin (enforces_expected_key_refs returns true), or leave expected_key_refs out.",
    )
}

fn envelope(batch: CryptoBatch) -> Response<CryptoBatch> {
    Response {
        data: batch,
        meta: Meta::default(),
    }
}

/// True for a 409 `K9O_ZK_STORE_FLOW_REQUIRED` (custody moved, or the ciphertext is client-side).
fn is_flow_required(e: &KError) -> bool {
    e.status == Some(409) && e.code == CODE_ZK_STORE_FLOW_REQUIRED
}

/// True when that 409 says the client route is the one to use.
fn wants_client_route(e: &KError) -> bool {
    is_flow_required(e) && e.context.get("route").and_then(Value::as_str) == Some("client")
}

fn mode_of(e: &KError) -> String {
    e.context
        .get("custodyMode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `client.crypto().crypts()`.
///
/// Registering a crypt makes `encrypt` accept the name; retiring it makes `encrypt` refuse the name.
/// Nothing here isolates, enforces or destroys anything: `decrypt` and `rewrap` never read the
/// registry, so data stays readable. Create and retire need `crypto:manage_crypts` (project admins
/// by default); a personal access token is refused on retire.
pub struct CryptsNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl CryptsNamespace<'_> {
    /// `GET /@<slug>/crypto/crypts`: the crypts you hold (all of them with a `*` grant on crypts,
    /// none for a member with no grant). Errors: 403 `K9O_ACCESS_DENIED`, 503 if the registry cannot
    /// be read. An answer without a `crypts` list is `K9O_INTERNAL_ERROR`.
    pub async fn list(&self) -> Result<Response<CryptList>, KError> {
        let slug = require_project(self.config, "crypto")?;
        let r = request::<Value, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "crypto/crypts"),
            None,
            None,
        )
        .await?;
        let arr = r
            .data
            .get("crypts")
            .and_then(Value::as_array)
            .ok_or_else(|| crypt_malformed("list", "without crypts"))?;
        let crypts = arr
            .iter()
            .map(|c| crypt_of(c, "list"))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response {
            data: CryptList { crypts },
            meta: r.meta,
        })
    }

    /// `POST /@<slug>/crypto/crypts`: register `name` so `encrypt` accepts it. The name is 1 to 64
    /// characters of a-z, 0-9 and `-`, and not `actions`; a bad name is `K9O_INVALID_FIELD_VALUE`
    /// (field `name`) before anything is sent. Only `name` is sent. Needs `crypto:manage_crypts`.
    /// Errors: 409 `K9O_CRYPT_ALREADY_EXISTS`, 409 `K9O_CRYPT_LIMIT_EXCEEDED` (256 per project,
    /// retired ones counted), 409 `K9O_CRYPT_RETIRED`. Never retried.
    pub async fn create(&self, name: &str) -> Result<Response<Crypt>, KError> {
        let slug = require_project(self.config, "crypto")?;
        check_crypt_name(name, "create")?;
        let body = json!({ "name": name });
        let r = request::<Value, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "crypto/crypts"),
            Some(&body),
            None,
        )
        .await?;
        Ok(Response {
            data: crypt_of(&r.data, "create")?,
            meta: r.meta,
        })
    }

    /// `POST /@<slug>/crypto/crypts/<name>/retire` (no body): `encrypt` then refuses the name.
    /// Needs `crypto:manage_crypts`; any personal access token is refused with `K9O_RBAC_DENY`.
    /// Errors: 404 `K9O_CRYPT_NOT_FOUND`, 409 `K9O_CRYPT_RETIRED`. Never retried.
    pub async fn retire(&self, name: &str) -> Result<Response<Crypt>, KError> {
        let slug = require_project(self.config, "crypto")?;
        check_crypt_name(name, "retire")?;
        let r = request::<Value, ()>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("crypto/crypts/{name}/retire")),
            None,
            None,
        )
        .await?;
        Ok(Response {
            data: crypt_of(&r.data, "retire")?,
            meta: r.meta,
        })
    }
}

/// `client.crypto()`.
pub struct CryptoNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl CryptoNamespace<'_> {
    async fn call(
        &self,
        verb: &str,
        crypt: &str,
        items: &[Value],
        epoch: Option<&str>,
    ) -> Result<Response<CryptoBatch>, KError> {
        let slug = require_project(self.config, "crypto")?;
        let mut body = json!({ "crypt": crypt, "items": items });
        if let Some(e) = epoch {
            body["custodyEpoch"] = json!(e);
        }
        // POST: the transport does not retry it, and neither does the SDK after a 5xx.
        request::<CryptoBatch, Value>(
            self.config,
            Method::POST,
            &project_path(slug, &format!("crypto/{verb}")),
            Some(&body),
            None,
        )
        .await
    }

    /// `POST /@<slug>/crypto/encrypt`: seal 1 to 100 items under the project's key. The answer holds
    /// one `ciphertext` per item, in order. Plaintext is at most 64 KiB per item and 1 MiB per call
    /// (checked here as `K9O_PAYLOAD_TOO_LARGE`); `crypt` is a registered name that matches `[a-z0-9-]{1,64}` and is not `actions`; `context` is at
    /// most 256 bytes.
    ///
    /// Gateway answers that pass through as `KError` unchanged: 404 `K9O_CRYPT_NOT_FOUND` (the name is
    /// not registered) and 409 `K9O_CRYPT_RETIRED` (the name is retired); decrypt and rewrap are not
    /// refused for either.
    ///
    /// Not live on every gateway yet: a 404 on `/crypto/*` means the gateway does not serve it. Fails
    /// closed: when the custody check fails or is unknown, nothing is sent.
    pub async fn encrypt(
        &self,
        crypt: &str,
        items: &[PlainItem],
    ) -> Result<Response<CryptoBatch>, KError> {
        require_project(self.config, "crypto")?;
        check_crypt(crypt)?;
        check_count(items.len())?;
        let mut total = 0usize;
        let mut wire = Vec::with_capacity(items.len());
        for it in items {
            check_context(&it.context)?;
            if it.plaintext.len() > MAX_ITEM_BYTES {
                return Err(too_large("a plaintext is over 64 KiB"));
            }
            total += it.plaintext.len();
            if total > MAX_CALL_BYTES {
                return Err(too_large("the plaintext of a call is over 1 MiB"));
            }
            let mut o = json!({ "plaintext": STANDARD.encode(&it.plaintext) });
            if let Some(c) = &it.context {
                o["context"] = json!(c);
            }
            wire.push(o);
        }
        // A fresh custody check on every call, never cached: a stale "platform managed" after a move
        // to your own key would send plaintext to the platform. One re-route at most.
        let mut attempt = 0;
        loop {
            let mode = self.read_mode(Some(crypt)).await?.data;
            let m = mode.custody_mode.as_str();
            let unknown = |msg: &str| {
                let mut e = KError::new(CODE_CUSTODY_MODE_UNKNOWN, msg).with_context("custodyMode", m);
                if let Some(Value::String(r)) = &mode.route {
                    e = e.with_context("route", r.as_str());
                }
                e
            };
            let server_route = match &mode.route {
                None => None,
                Some(Value::String(r)) if r == "server" => Some(true),
                Some(Value::String(r)) if r == "client" => Some(false),
                Some(_) => {
                    return Err(unknown(
                        "crypto: the project's route is not recognised; nothing was sent",
                    ))
                }
            };
            let to_server = if is_server_mode(m) {
                true
            } else if is_client_mode(m) {
                server_route == Some(true)
            } else {
                return Err(unknown(
                    "crypto: the project's custody mode is missing or not recognised; nothing was sent",
                ));
            };
            if to_server {
                if mode.epoch.is_empty() {
                    return Err(unknown(
                        "crypto: the project's custody epoch is missing; nothing was sent",
                    ));
                }
                match self.call("encrypt", crypt, &wire, Some(&mode.epoch)).await {
                    Err(e) if is_flow_required(&e) && attempt == 0 => attempt += 1,
                    other => return other,
                }
            } else {
                let core = self.need_core(m)?;
                return core
                    .encrypt(crypt, items, &self.core_options())
                    .await
                    .map(envelope);
            }
        }
    }

    /// The crypt registry: `list`, `create` and `retire`.
    pub fn crypts(&self) -> CryptsNamespace<'_> {
        CryptsNamespace {
            config: self.config,
        }
    }

    /// `GET /@<slug>/crypto/mode`: the project's custody mode and the epoch that changes with every
    /// custody transition. Any caller with a `crypto` grant may read it.
    pub async fn mode(&self) -> Result<Response<CryptoMode>, KError> {
        self.read_mode(None).await
    }

    /// `mode` about one crypt (`?crypt=<crypt>`): a personal access token or workload policy scoped to
    /// named crypts is refused without one. A bad name is `K9O_INVALID_FIELD_VALUE`, before anything is sent.
    pub async fn mode_for(&self, crypt: &str) -> Result<Response<CryptoMode>, KError> {
        check_crypt(crypt)?;
        self.read_mode(Some(crypt)).await
    }

    /// The mode read with an optional `?crypt=`: `encrypt` names the crypt it is about to use, so a
    /// caller whose grant names only specific crypts is authorized on that one.
    async fn read_mode(&self, crypt: Option<&str>) -> Result<Response<CryptoMode>, KError> {
        let slug = require_project(self.config, "crypto")?;
        let query: Option<Vec<(String, String)>> =
            crypt.map(|c| vec![("crypt".to_string(), c.to_string())]);
        request::<CryptoMode, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "crypto/mode"),
            None,
            query.as_deref(),
        )
        .await
    }

    /// `POST /@<slug>/crypto/federated-token`: a short-lived token that your own cloud KMS or HSM
    /// can trust. `crypt` is a registered name that matches `[a-z0-9-]{1,64}` and is not `actions`; `op` is `"encrypt"`, `"decrypt"` or
    /// `"rewrap"`. Both are checked here (`K9O_INVALID_FIELD_VALUE`) before anything is sent. A 409
    /// `K9O_CUSTODY_SERVER_ROUTE` (its context `route` is `server` or `client-idp`), 429 `K9O_RATE_LIMITED` or 503 `K9O_KEY_SERVICE_UNAVAILABLE` comes
    /// back as the `KError`. The SDK never retries it. An answer without the token or any of its
    /// fields is `K9O_INTERNAL_ERROR`. The token is never put in an error.
    pub async fn federated_token(
        &self,
        crypt: &str,
        op: &str,
    ) -> Result<Response<CryptoFederatedToken>, KError> {
        check_crypt(crypt)?;
        if !matches!(op, "encrypt" | "decrypt" | "rewrap") {
            return Err(invalid("op must be encrypt, decrypt or rewrap"));
        }
        let slug = require_project(self.config, "crypto")?;
        let body = json!({ "crypt": crypt, "op": op });
        // POST: the transport does not retry it.
        let r = request::<CryptoFederatedToken, Value>(
            self.config,
            Method::POST,
            &project_path(slug, "crypto/federated-token"),
            Some(&body),
            None,
        )
        .await?;
        let d = &r.data;
        if [
            &d.token,
            &d.expires_at,
            &d.issuer,
            &d.subject,
            &d.audience,
            &d.kid,
        ]
        .iter()
        .any(|f| f.is_empty())
        {
            return Err(KError::new(
                "K9O_INTERNAL_ERROR",
                "crypto: malformed federated-token answer: token, expiresAt, issuer, subject, audience and kid are all required",
            ));
        }
        Ok(r)
    }

    /// Register the client core: the hook the Kibo binding fills. It serves every call whose key is
    /// in your own KMS or HSM (BYOK, HYOK). Replaces any earlier one.
    pub fn use_client_core(&self, core: Arc<dyn ClientCore>) {
        if let Ok(mut g) = self.config.crypto.core.write() {
            *g = Some(core);
        }
    }

    fn core_options(&self) -> CoreOptions<'_> {
        CoreOptions {
            expected_key_refs: self.config.expected_key_refs.as_deref(),
        }
    }

    fn core(&self) -> Option<Arc<dyn ClientCore>> {
        self.config.crypto.core.read().ok().and_then(|g| g.clone())
    }

    /// The registered core for the client route, or the refusal: none registered, or a pin is set
    /// and the core does not declare that it enforces it.
    fn need_core(&self, mode: &str) -> Result<Arc<dyn ClientCore>, KError> {
        let core = self.core().ok_or_else(|| core_unavailable(mode))?;
        if self.config.expected_key_refs.is_some() && !core.enforces_expected_key_refs() {
            return Err(key_pin_unenforced(mode));
        }
        Ok(core)
    }

    fn sealed(crypt: &str, items: &[SealedItem]) -> Result<Vec<Value>, KError> {
        check_crypt(crypt)?;
        check_count(items.len())?;
        items
            .iter()
            .map(|it| {
                check_context(&it.context)?;
                if it.ciphertext.is_empty() {
                    return Err(invalid("ciphertext must not be empty"));
                }
                let mut o = json!({ "ciphertext": it.ciphertext });
                if let Some(c) = &it.context {
                    o["context"] = json!(c);
                }
                Ok(o)
            })
            .collect()
    }

    /// `POST /@<slug>/crypto/decrypt`: open 1 to 100 items. Give the same `crypt` and `context` as
    /// on encrypt. Every cannot-open is one answer, 400 `K9O_ENVELOPE_INTEGRITY_FAILURE`, so it cannot
    /// tell you why. 409 `K9O_SEALED_UNDER_RETIRED_KEY` means [`Self::rewrap`] the ciphertext
    /// (`context.resealableUntil` while the window is open).
    pub async fn decrypt(
        &self,
        crypt: &str,
        items: &[SealedItem],
    ) -> Result<Response<CryptoBatch>, KError> {
        require_project(self.config, "crypto")?;
        let wire = Self::sealed(crypt, items)?;
        match self.call("decrypt", crypt, &wire, None).await {
            Err(e) if wants_client_route(&e) => {
                let core = self.need_core(&mode_of(&e))?;
                core.decrypt(crypt, items, &self.core_options())
                    .await
                    .map(envelope)
            }
            other => other,
        }
    }

    /// `POST /@<slug>/crypto/rewrap`: open each item and seal it again under the project's current
    /// key in one call; the plaintext never leaves the platform.
    pub async fn rewrap(
        &self,
        crypt: &str,
        items: &[SealedItem],
    ) -> Result<Response<CryptoBatch>, KError> {
        require_project(self.config, "crypto")?;
        let wire = Self::sealed(crypt, items)?;
        match self.call("rewrap", crypt, &wire, None).await {
            Err(e) if wants_client_route(&e) => {
                let core = self.need_core(&mode_of(&e))?;
                core.rewrap(crypt, items, &self.core_options())
                    .await
                    .map(envelope)
            }
            other => other,
        }
    }
}
