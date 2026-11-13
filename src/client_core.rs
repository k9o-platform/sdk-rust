//! The client core for `client.crypto()` on a BYOK or HYOK project: opens and seals data on this
//! machine, against your own KMS or HSM, so the platform never sees plaintext or the key.
//!
//! There is no cryptography in this module. Sealing, opening, the key pin and the token exchange
//! all run in the audited Kibo core (`kibo-kms-rest-ffi`, `zk-bundle-v2.16.3` or later). This module
//! only sequences them:
//!
//! ```text
//! federated token (platform) -> acquire_kms_credential (your cloud) -> app_crypto_seal / app_crypto_open (your KMS)
//! ```
//!
//! The Kibo core is not a dependency of this crate. [`AppCore`] is the seam: a trait with plain
//! structs that mirror the core's `AppSealInput`, `AppOpenInput`, `FfiCredential`,
//! `FfiFederationSpec`, `FfiFederationInput` and `FfiProviderConfig` (snake_case). The ready
//! adapter, `KiboCore` in the `k9o-kibo-core` crate (the `kibo-core` directory), ships with the
//! next Kibo core release; until then an application implements [`AppCore`] itself by calling the
//! core's `app_crypto_seal`, `app_crypto_open`, `acquire_kms_credential` and `core_capabilities`,
//! and gives the core's error as the text `"<CODE>: <message>"`. The one field renamed on the way
//! is the Vault JWT auth mount:
//! [`FederationSpec::Vault`] calls it `jwt_auth_mount`, the core calls it `mount`.
//!
//! Everything that picks a key, a provider, a role, a region or an endpoint comes from the
//! configuration you pass to [`KiboClientCore::new`]. Nothing comes from the platform's answer: the
//! token response's `provider` and `project_id` are checked against your configuration, and a
//! mismatch is refused, never followed. Register the core with
//! [`CryptoNamespace::use_client_core`](crate::crypto::CryptoNamespace::use_client_core).

use crate::crypto::{
    check_expected_key_refs, key_ref_matches, ClientCore, CoreOptions, CryptoBatch,
    CryptoFederatedToken, CryptoItemResult, KeyRefProvider, PlainItem, SealedItem,
    CODE_KEY_PIN_UNENFORCED,
};
use crate::kerror::KError;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use serde_json::Map;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Raised by this client when the exchange runs past its deadline.
pub const CODE_EXCHANGE_TIMEOUT: &str = "KIBO_EXCHANGE_TIMEOUT";
/// The Kibo core failed, or is older than `zk-bundle-v2.16.3`.
pub const CODE_WASM_ERROR: &str = "KIBO_WASM_ERROR";

/// Refresh a credential this long before it expires.
const SKEW_SECS: u64 = 60;

/// The cloud provider name the Kibo core uses on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum KmsProvider {
    #[serde(rename = "aws-kms")]
    AwsKms,
    #[serde(rename = "gcp-kms")]
    GcpKms,
    #[serde(rename = "azure-kv")]
    AzureKv,
    #[serde(rename = "hashicorp-vault")]
    HashicorpVault,
}

impl KmsProvider {
    fn of(p: KeyRefProvider) -> Self {
        match p {
            KeyRefProvider::Aws => Self::AwsKms,
            KeyRefProvider::Gcp => Self::GcpKms,
            KeyRefProvider::Azure => Self::AzureKv,
            KeyRefProvider::Vault => Self::HashicorpVault,
        }
    }
}

/// Custody posture of the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
    Byok,
    Hyok,
}

/// Where your key lives and how to reach it for the token exchange. All of it is yours: the
/// platform never supplies any of these. Mirrors `FfiFederationSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum FederationSpec {
    /// AWS STS `AssumeRoleWithWebIdentity`, scoped to the one key.
    Aws {
        role_arn: String,
        session_name: String,
        region: String,
        key_arn: String,
        /// 900 to 43200.
        duration_secs: u32,
    },
    /// GCP workload identity federation.
    Gcp { audience: String },
    /// Microsoft Entra workload identity federation.
    Azure {
        tenant_id: String,
        client_id: String,
        scope: String,
    },
    /// Vault JWT login.
    Vault {
        vault_addr: String,
        /// The JWT auth mount: the name Vault serves the JWT login under, `jwt` for `auth/jwt`
        /// (not the transit mount, which comes from the key reference). The core calls
        /// `{vault_addr}/v1/auth/{jwt_auth_mount}/login`. The core's own field for it is `mount`.
        jwt_auth_mount: String,
        role: String,
    },
}

impl FederationSpec {
    fn provider(&self) -> KeyRefProvider {
        match self {
            Self::Aws { .. } => KeyRefProvider::Aws,
            Self::Gcp { .. } => KeyRefProvider::Gcp,
            Self::Azure { .. } => KeyRefProvider::Azure,
            Self::Vault { .. } => KeyRefProvider::Vault,
        }
    }
}

/// The exact key this application uses. Mirrors `FfiKeyRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppKeyRef {
    pub version: u64,
    pub external_ref: String,
}

/// Provider-specific initialisation: region for AWS, address for Vault. Mirrors `FfiProviderConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProviderConfig {
    pub aws_region: Option<String>,
    pub vault_address: Option<String>,
}

/// A credential for your own KMS. Mirrors `FfiCredential`. `secret` is redacted in `Debug`.
#[derive(Clone, Serialize)]
pub struct AppCredential {
    pub secret: String,
    pub expires_at_unix_secs: u64,
}

impl std::fmt::Debug for AppCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredential")
            .field("secret", &"<redacted>")
            .field("expires_at_unix_secs", &self.expires_at_unix_secs)
            .finish()
    }
}

/// Input of `app_crypto_seal` (one item). `Debug` redacts the credential and the plaintext.
#[derive(Clone, Serialize)]
pub struct AppSealInput {
    pub provider: KmsProvider,
    pub posture: Posture,
    pub kms_cred: AppCredential,
    pub key_ref: AppKeyRef,
    pub provider_config: ProviderConfig,
    pub project_id: String,
    pub crypt: String,
    pub context: Option<String>,
    pub plaintext: Vec<u8>,
    pub expected_key_refs: Vec<String>,
}

impl std::fmt::Debug for AppSealInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppSealInput")
            .field("provider", &self.provider)
            .field("kms_cred", &self.kms_cred)
            .field("key_ref", &self.key_ref)
            .field("project_id", &self.project_id)
            .field("crypt", &self.crypt)
            .field("plaintext", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Input of `app_crypto_open` (one item). `Debug` redacts the credential.
#[derive(Clone, Serialize)]
pub struct AppOpenInput {
    pub kms_cred: AppCredential,
    pub provider_config: ProviderConfig,
    pub project_id: String,
    pub crypt: String,
    pub context: Option<String>,
    pub container: Vec<u8>,
    pub expected_key_refs: Vec<String>,
}

impl std::fmt::Debug for AppOpenInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppOpenInput")
            .field("kms_cred", &self.kms_cred)
            .field("project_id", &self.project_id)
            .field("crypt", &self.crypt)
            .finish_non_exhaustive()
    }
}

/// Input of `acquire_kms_credential`. `Debug` redacts the identity token.
#[derive(Clone, Serialize)]
pub struct FederationInput {
    pub spec: FederationSpec,
    pub identity_token: String,
    pub now_unix_secs: u64,
}

impl std::fmt::Debug for FederationInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FederationInput")
            .field("spec", &self.spec)
            .field("identity_token", &"<redacted>")
            .field("now_unix_secs", &self.now_unix_secs)
            .finish()
    }
}

/// The Kibo core's app-crypto surface. An error is the core's text, `"<CODE>: <message>"`;
/// `UPSTREAM_KMS_ERROR` is your own KMS failing.
#[async_trait]
pub trait AppCore: Send + Sync {
    /// The core's `core_capabilities()`. Must contain `"expected_key_refs"`.
    fn capabilities(&self) -> Vec<String>;
    /// The names of the functions this core lacks (`app_crypto_seal`, `app_crypto_open`,
    /// `acquire_kms_credential`, `core_capabilities`). An adapter over a core older than
    /// `zk-bundle-v2.16.3` lists them; the default is none.
    fn missing_exports(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// `app_crypto_seal`: the container bytes.
    async fn seal(&self, input: AppSealInput) -> Result<Vec<u8>, String>;
    /// `app_crypto_open`: the plaintext.
    async fn open(&self, input: AppOpenInput) -> Result<Vec<u8>, String>;
    /// `acquire_kms_credential`.
    async fn acquire(&self, input: FederationInput) -> Result<AppCredential, String>;
}

/// Which operation a federated token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenOp {
    Encrypt,
    Decrypt,
    Rewrap,
}

impl TokenOp {
    /// The wire name: `encrypt`, `decrypt` or `rewrap`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Encrypt => "encrypt",
            Self::Decrypt => "decrypt",
            Self::Rewrap => "rewrap",
        }
    }
}

/// What the core asks the platform for on each call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedTokenRequest {
    pub crypt: String,
    pub op: TokenOp,
}

/// The future a token source returns.
pub type TokenFuture = Pin<Box<dyn Future<Output = Result<CryptoFederatedToken, KError>> + Send>>;
/// `|req| client.crypto().federated_token(&req.crypt, req.op.as_str())`, boxed, with the client
/// shared (for example in an `Arc`).
pub type FederatedTokenFn = Arc<dyn Fn(FederatedTokenRequest) -> TokenFuture + Send + Sync>;

/// Configuration of [`KiboClientCore`].
pub struct KiboClientCoreConfig {
    /// Your cloud. Must equal the cloud of `federation`.
    pub provider: KeyRefProvider,
    pub posture: Posture,
    /// The project id (`proj_...`). Bound into what is sealed. Leave `None` only to take the token
    /// response's value; a value given here is never overridden.
    pub project_id: Option<String>,
    /// The exact key this application uses.
    pub key_ref: AppKeyRef,
    /// Region for AWS, address for Vault.
    pub provider_config: ProviderConfig,
    /// How to exchange the platform's identity token for a credential on your cloud.
    pub federation: FederationSpec,
    /// The keys this application allows. Required and non-empty: a core with no pin opens whatever
    /// key a stored container names, so this client never calls without one.
    pub expected_key_refs: Vec<String>,
    /// Fetches the federated token, on every call.
    pub federated_token: FederatedTokenFn,
    /// The Kibo core.
    pub core: Arc<dyn AppCore>,
    /// How long the exchange may take before it is abandoned. Default 30 s.
    pub exchange_deadline: Duration,
    /// The host clock, Unix seconds. Default: the system clock.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl KiboClientCoreConfig {
    /// A configuration with the required parts; the rest are defaults.
    pub fn new(
        provider: KeyRefProvider,
        posture: Posture,
        key_ref: AppKeyRef,
        federation: FederationSpec,
        expected_key_refs: Vec<String>,
        federated_token: FederatedTokenFn,
        core: Arc<dyn AppCore>,
    ) -> Self {
        Self {
            provider,
            posture,
            project_id: None,
            key_ref,
            provider_config: ProviderConfig::default(),
            federation,
            expected_key_refs,
            federated_token,
            core,
            exchange_deadline: Duration::from_secs(30),
            now: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }
    }
}

impl std::fmt::Debug for KiboClientCoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiboClientCoreConfig")
            .field("provider", &self.provider)
            .field("posture", &self.posture)
            .field("project_id", &self.project_id)
            .field("key_ref", &self.key_ref)
            .finish_non_exhaustive()
    }
}

fn bad(message: &str, field: &str) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message).with_context("field", field)
}

fn provider_name(p: KeyRefProvider) -> &'static str {
    match p {
        KeyRefProvider::Aws => "aws",
        KeyRefProvider::Gcp => "gcp",
        KeyRefProvider::Azure => "azure",
        KeyRefProvider::Vault => "vault",
    }
}

/// The core's error text is `"<CODE>: <message>"`; anything else is a `KIBO_WASM_ERROR`.
fn map_core_error(raw: &str) -> KError {
    if let Some((head, rest)) = raw.split_once(':') {
        let mut cs = head.chars();
        let code_ok = matches!(cs.next(), Some(c) if c.is_ascii_uppercase())
            && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if code_ok {
            return KError::new(head, rest.trim_start());
        }
    }
    KError::new(CODE_WASM_ERROR, raw)
}

fn failure(e: &KError) -> CryptoItemResult {
    CryptoItemResult {
        ciphertext: None,
        plaintext: None,
        code: Some(e.code.clone()),
        message: Some(e.message.clone()),
        extra: Map::new(),
    }
}

fn item_ok(ciphertext: Option<String>, plaintext: Option<String>) -> CryptoItemResult {
    CryptoItemResult {
        ciphertext,
        plaintext,
        code: None,
        message: None,
        extra: Map::new(),
    }
}

/// The client core. Register it with `client.crypto().use_client_core(Arc::new(core))`.
pub struct KiboClientCore {
    cfg: KiboClientCoreConfig,
    pin: Vec<String>,
    creds: Mutex<HashMap<String, AppCredential>>,
    caps_checked: AtomicBool,
}

impl std::fmt::Debug for KiboClientCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiboClientCore")
            .field("config", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl KiboClientCore {
    /// Build the core. Refuses (`K9O_INVALID_FIELD_VALUE`) a provider that differs from the
    /// federation's, an empty or malformed pin, and a `key_ref` that is not in the pin.
    pub fn new(cfg: KiboClientCoreConfig) -> Result<Self, KError> {
        if cfg.provider != cfg.federation.provider() {
            return Err(bad(
                "provider and federation must be the same cloud",
                "provider",
            ));
        }
        if cfg.expected_key_refs.is_empty() {
            return Err(bad(
                "expected_key_refs is required: this client never seals or opens without a key pin",
                "expectedKeyRefs",
            ));
        }
        let pin = check_expected_key_refs(Some(cfg.expected_key_refs.clone()))?
            .ok_or_else(|| bad("expected_key_refs is required", "expectedKeyRefs"))?;
        if !key_ref_matches(cfg.provider, &cfg.key_ref.external_ref, &pin) {
            return Err(bad("key_ref is not in expected_key_refs", "keyRef"));
        }
        Ok(Self {
            cfg,
            pin,
            creds: Mutex::new(HashMap::new()),
            caps_checked: AtomicBool::new(false),
        })
    }

    fn effective_pin(&self, options: &CoreOptions<'_>) -> Result<Vec<String>, KError> {
        let p: Vec<String> = match options.expected_key_refs {
            Some(p) => p.to_vec(),
            None => self.pin.clone(),
        };
        if p.is_empty() {
            return Err(bad(
                "expected_key_refs is empty: refusing to call without a key pin",
                "expectedKeyRefs",
            ));
        }
        Ok(p)
    }

    /// Matches the configured key against this call's pin, before any token is fetched or credential exchanged.
    fn refuse_if_key_not_pinned(&self, pin: &[String], n: usize) -> Option<CryptoBatch> {
        if key_ref_matches(self.cfg.provider, &self.cfg.key_ref.external_ref, pin) {
            return None;
        }
        let e = KError::new(
            "K9O_CRYPTO_UNEXPECTED_KEY",
            "the configured key is not in this call's expected_key_refs; nothing was sent",
        );
        Some(CryptoBatch {
            items: (0..n).map(|_| failure(&e)).collect(),
        })
    }

    /// Checks the core's exports and capabilities before any token is fetched.
    fn checked_core(&self) -> Result<Arc<dyn AppCore>, KError> {
        let core = self.cfg.core.clone();
        if let Some(missing) = core.missing_exports().first() {
            return Err(KError::new(
                CODE_WASM_ERROR,
                format!("the Kibo core does not export {missing}; it is older than zk-bundle-v2.16.3"),
            )
            .with_context("missing", *missing)
            .with_recommendation("Use a build of the Kibo core at zk-bundle-v2.16.3 or later."));
        }
        if !self.caps_checked.load(Ordering::Acquire) {
            if !core.capabilities().iter().any(|c| c == "expected_key_refs") {
                return Err(KError::new(
                    CODE_KEY_PIN_UNENFORCED,
                    "the Kibo core does not declare that it enforces expected_key_refs; nothing was sent",
                )
                .with_recommendation("Use a build of the Kibo core at zk-bundle-v2.16.3 or later."));
            }
            self.caps_checked.store(true, Ordering::Release);
        }
        Ok(core)
    }

    async fn credential(
        &self,
        core: &Arc<dyn AppCore>,
        crypt: &str,
        op: TokenOp,
    ) -> Result<(AppCredential, String), KError> {
        // A token is fetched on every call: it is bound to the call's crypt and op.
        let t = (self.cfg.federated_token)(FederatedTokenRequest {
            crypt: crypt.to_string(),
            op,
        })
        .await?;
        if let Some(named) = t.provider.as_deref() {
            if named != provider_name(self.cfg.provider) {
                return Err(bad(
                    &format!(
                        "the platform named provider \"{named}\"; this client is configured for \"{}\"",
                        provider_name(self.cfg.provider)
                    ),
                    "provider",
                ));
            }
        }
        if let (Some(named), Some(configured)) =
            (t.project_id.as_deref(), self.cfg.project_id.as_deref())
        {
            if named != configured {
                return Err(bad(
                    "the platform named a different project than the one configured",
                    "projectId",
                ));
            }
        }
        let project_id = self
            .cfg
            .project_id
            .clone()
            .or_else(|| t.project_id.clone())
            .ok_or_else(|| {
                bad(
                    "project_id is not configured and the platform did not name one",
                    "projectId",
                )
            })?;

        let cache_key = format!("{crypt}\u{0}{}\u{0}{}", op.as_str(), t.subject);
        let now = (self.cfg.now)();
        if let Some(c) = self.creds.lock().unwrap().get(&cache_key) {
            if c.expires_at_unix_secs.saturating_sub(SKEW_SECS) > now {
                return Ok((c.clone(), project_id));
            }
        }
        let fut = core.acquire(FederationInput {
            spec: self.cfg.federation.clone(),
            identity_token: t.token.clone(),
            now_unix_secs: now,
        });
        let cred = match tokio::time::timeout(self.cfg.exchange_deadline, fut).await {
            Ok(Ok(c)) => c,
            Ok(Err(raw)) => return Err(map_core_error(&raw)),
            Err(_) => {
                return Err(KError::new(
                    CODE_EXCHANGE_TIMEOUT,
                    format!(
                        "the token exchange with your cloud did not finish within {} ms",
                        self.cfg.exchange_deadline.as_millis()
                    ),
                )
                .with_context("deadlineMs", self.cfg.exchange_deadline.as_millis() as u64)
                .with_recommendation(
                    "Check that your cloud's token endpoint is reachable from this host.",
                ))
            }
        };
        self.creds.lock().unwrap().insert(cache_key, cred.clone());
        Ok((cred, project_id))
    }

    fn provider_config(&self) -> ProviderConfig {
        self.cfg.provider_config.clone()
    }

    #[allow(clippy::too_many_arguments)]
    async fn seal(
        &self,
        core: &Arc<dyn AppCore>,
        cred: &AppCredential,
        project_id: &str,
        crypt: &str,
        plaintext: &[u8],
        context: Option<&str>,
        pin: &[String],
    ) -> Result<String, KError> {
        let container = core
            .seal(AppSealInput {
                provider: KmsProvider::of(self.cfg.provider),
                posture: self.cfg.posture,
                kms_cred: cred.clone(),
                key_ref: self.cfg.key_ref.clone(),
                provider_config: self.provider_config(),
                project_id: project_id.to_string(),
                crypt: crypt.to_string(),
                context: context.map(str::to_string),
                plaintext: plaintext.to_vec(),
                expected_key_refs: pin.to_vec(),
            })
            .await
            .map_err(|e| map_core_error(&e))?;
        Ok(STANDARD.encode(container))
    }

    #[allow(clippy::too_many_arguments)]
    async fn open(
        &self,
        core: &Arc<dyn AppCore>,
        cred: &AppCredential,
        project_id: &str,
        crypt: &str,
        ciphertext: &str,
        context: Option<&str>,
        pin: &[String],
    ) -> Result<Vec<u8>, KError> {
        let container = STANDARD
            .decode(ciphertext)
            .map_err(|_| KError::new(CODE_WASM_ERROR, "the ciphertext is not valid base64"))?;
        core.open(AppOpenInput {
            kms_cred: cred.clone(),
            provider_config: self.provider_config(),
            project_id: project_id.to_string(),
            crypt: crypt.to_string(),
            context: context.map(str::to_string),
            container,
            expected_key_refs: pin.to_vec(),
        })
        .await
        .map_err(|e| map_core_error(&e))
    }
}

#[async_trait]
impl ClientCore for KiboClientCore {
    fn enforces_expected_key_refs(&self) -> bool {
        true
    }

    async fn encrypt(
        &self,
        crypt: &str,
        items: &[PlainItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        let pin = self.effective_pin(options)?;
        if let Some(r) = self.refuse_if_key_not_pinned(&pin, items.len()) {
            return Ok(r);
        }
        let core = self.checked_core()?;
        let (cred, project_id) = self.credential(&core, crypt, TokenOp::Encrypt).await?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let r = self
                .seal(
                    &core,
                    &cred,
                    &project_id,
                    crypt,
                    &it.plaintext,
                    it.context.as_deref(),
                    &pin,
                )
                .await;
            out.push(match r {
                Ok(ct) => item_ok(Some(ct), None),
                Err(e) => failure(&e),
            });
        }
        Ok(CryptoBatch { items: out })
    }

    async fn decrypt(
        &self,
        crypt: &str,
        items: &[SealedItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        let pin = self.effective_pin(options)?;
        if let Some(r) = self.refuse_if_key_not_pinned(&pin, items.len()) {
            return Ok(r);
        }
        let core = self.checked_core()?;
        let (cred, project_id) = self.credential(&core, crypt, TokenOp::Decrypt).await?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let r = self
                .open(
                    &core,
                    &cred,
                    &project_id,
                    crypt,
                    &it.ciphertext,
                    it.context.as_deref(),
                    &pin,
                )
                .await;
            out.push(match r {
                Ok(pt) => item_ok(None, Some(STANDARD.encode(pt))),
                Err(e) => failure(&e),
            });
        }
        Ok(CryptoBatch { items: out })
    }

    async fn rewrap(
        &self,
        crypt: &str,
        items: &[SealedItem],
        options: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        let pin = self.effective_pin(options)?;
        if let Some(r) = self.refuse_if_key_not_pinned(&pin, items.len()) {
            return Ok(r);
        }
        let core = self.checked_core()?;
        let (cred, project_id) = self.credential(&core, crypt, TokenOp::Rewrap).await?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let r = async {
                let pt = self
                    .open(
                        &core,
                        &cred,
                        &project_id,
                        crypt,
                        &it.ciphertext,
                        it.context.as_deref(),
                        &pin,
                    )
                    .await?;
                self.seal(
                    &core,
                    &cred,
                    &project_id,
                    crypt,
                    &pt,
                    it.context.as_deref(),
                    &pin,
                )
                .await
            }
            .await;
            out.push(match r {
                Ok(ct) => item_ok(Some(ct), None),
                Err(e) => failure(&e),
            });
        }
        Ok(CryptoBatch { items: out })
    }
}
