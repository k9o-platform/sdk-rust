//! Rampart namespace: `client.rampart().classify / jwks / verify_receipt`.
//!
//! Wire mapping:
//!   - `rampart().classify(input, label)` -> `POST /@<slug>/rampart/classify` with `{ input, label? }`
//!   - `rampart().jwks()`                  -> `GET /rampart/jwks` (public; no project prefix)
//!
//! Rampart is advisory. `classify` returns a verdict or an error and blocks nothing; the caller
//! decides what to do with it. It fails closed in the sense that an unavailable classifier answers
//! 503 `K9O_RAMPART_CLASSIFIER_UNAVAILABLE`, never a default "clear". The SDK does not check the
//! input before sending it; input that cannot be canonicalised is answered with a typed 400.
//!
//! The verdict comes with a signed receipt (an EdDSA JWS). [`RampartNamespace::verify_receipt`]
//! checks it locally, in this order, and any failure is a typed [`ReceiptRefusal`] with no
//! partial verdict: the signing key is picked by the header `kid` from the key feed; `alg` must be
//! `EdDSA`; `typ` must be `k9o.rampart.classify.v1`; the signature must verify; `iss` must equal
//! the issuer the key feed advertises (or the `issuer` option) and `project_id` must equal the
//! project id (the `project_id` option, else the one the classify result carries); the receipt must
//! be inside its `iat`/`exp` window (default skew 60 s); and `input_sha256` must equal the SHA-256
//! of the platform's canonical form (see [`crate::jcs`]) of the exact input you pass. The verdict returned is read from the verified claims, never from the unsigned body.

use crate::ed25519;
use crate::envelope::Response;
use crate::hash::{hex, sha256};
use crate::http::{request, request_bare, HttpClientConfig};
use crate::jcs;
use crate::kerror::KError;
use crate::stream::require_project;
use crate::url_grammar::project_path;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::sync::{Arc, Mutex};

/// The `typ` every classification receipt carries.
pub const RECEIPT_TYP: &str = "k9o.rampart.classify.v1";

/// The verdict of `classify`, plus the receipt exactly as received.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    /// `clear`, `suspicious` or `malicious`. Uncertain input comes back `suspicious`.
    pub classification: String,
    pub risk_score: i64,
    /// Basis points, 0 to 10000.
    pub confidence: i64,
    pub categories: Vec<String>,
    pub calibration_version: String,
    /// The signed receipt (a compact JWS), unchanged. Pass it to `verify_receipt`.
    pub classification_receipt: String,
    /// The project's id (`proj_...`), when the server returns it. `verify_receipt` uses it as the
    /// expected project when you pass this result.
    #[serde(default)]
    pub project_id: Option<String>,
}

/// What `verify_receipt` accepts as the receipt: a [`Classification`] (which carries the receipt
/// and its project id) or a bare receipt string (`&str` or `String`).
pub trait ReceiptSource {
    /// The compact JWS.
    fn receipt(&self) -> &str;
    /// The project id that came with the receipt, if any.
    fn project_id(&self) -> Option<&str>;
}

impl ReceiptSource for str {
    fn receipt(&self) -> &str {
        self
    }
    fn project_id(&self) -> Option<&str> {
        None
    }
}

impl ReceiptSource for String {
    fn receipt(&self) -> &str {
        self
    }
    fn project_id(&self) -> Option<&str> {
        None
    }
}

impl ReceiptSource for Classification {
    fn receipt(&self) -> &str {
        &self.classification_receipt
    }
    fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }
}

/// A public key of the key feed (RFC 7517). Other members of the key object are ignored.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub crv: Option<String>,
    /// The public key, base64url.
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
}

/// The key feed: `{ keys: [...], issuer }`. `issuer` is the deployment's issuer for receipts
/// (`<issuer>/rampart`); it is `None` when the feed does not advertise one.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct JwkSet {
    pub keys: Vec<Jwk>,
    #[serde(default)]
    pub issuer: Option<String>,
}

/// Why a receipt was refused. The set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptRefusalReason {
    /// Not a three-part compact JWS, or a part or required claim is missing or unreadable.
    Malformed,
    /// No key with the receipt's `kid`, or that key is not an Ed25519 public key.
    KeyUnknown,
    /// The header `alg` is not `EdDSA`.
    Alg,
    /// The header `typ` is not `k9o.rampart.classify.v1`.
    Typ,
    /// The signature does not verify.
    Signature,
    /// `iss` is not the expected issuer.
    Issuer,
    /// `project_id` is not the expected project.
    Project,
    /// No expected project is known: a bare receipt string was passed without the `project_id` option.
    ProjectUnknown,
    /// The receipt is past its `exp` (plus skew).
    Expired,
    /// The receipt's `iat` is in the future (beyond skew).
    NotYetValid,
    /// `input_sha256` does not match the input that was passed.
    InputHash,
    /// The key feed could not be fetched.
    KeyFeed,
}

impl ReceiptRefusalReason {
    /// `malformed`, `key_unknown`, `alg`, `typ`, `signature`, `issuer`, `project`, `project_unknown`,
    /// `expired`, `not_yet_valid`, `input_hash` or `key_feed`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReceiptRefusalReason::Malformed => "malformed",
            ReceiptRefusalReason::KeyUnknown => "key_unknown",
            ReceiptRefusalReason::Alg => "alg",
            ReceiptRefusalReason::Typ => "typ",
            ReceiptRefusalReason::Signature => "signature",
            ReceiptRefusalReason::Issuer => "issuer",
            ReceiptRefusalReason::Project => "project",
            ReceiptRefusalReason::ProjectUnknown => "project_unknown",
            ReceiptRefusalReason::Expired => "expired",
            ReceiptRefusalReason::NotYetValid => "not_yet_valid",
            ReceiptRefusalReason::InputHash => "input_hash",
            ReceiptRefusalReason::KeyFeed => "key_feed",
        }
    }
}

/// A refused receipt: the closed `reason` and a short `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptRefusal {
    pub reason: ReceiptRefusalReason,
    pub detail: String,
}

impl fmt::Display for ReceiptRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "classification receipt refused ({}): {}",
            self.reason.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for ReceiptRefusal {}

fn refuse<T>(reason: ReceiptRefusalReason, detail: impl Into<String>) -> Result<T, ReceiptRefusal> {
    Err(ReceiptRefusal {
        reason,
        detail: detail.into(),
    })
}

/// The verdict read from a verified receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedReceipt {
    pub classification: String,
    /// `risk_bp` claim.
    pub risk_bp: i64,
    /// `confidence_bp` claim.
    pub confidence_bp: i64,
    pub categories: Vec<String>,
    pub calibration_version: String,
    /// `windows` claim: how many windows of the input were scored (1 when it fits one window).
    /// A receipt without the claim reads 1.
    pub windows: i64,
    pub project_id: String,
    pub input_sha256: String,
    pub issuer: String,
    pub issued_at: i64,
    pub expires_at: i64,
    /// The `kid` of the key that signed it.
    pub kid: String,
}

/// Options for `verify_receipt`.
#[derive(Clone, Default)]
pub struct VerifyOptions {
    /// A key set you already fetched. When set, the feed is never fetched.
    pub key_set: Option<JwkSet>,
    /// The expected `iss`. Default: the `issuer` the key feed advertises. When this is set it wins.
    pub issuer: Option<String>,
    /// The expected `project_id` (`proj_...`). Default: the project id the classify result you pass
    /// carries. A bare receipt string with neither is refused as `project_unknown`.
    pub project_id: Option<String>,
    /// Allowed clock difference in seconds. Default 60.
    pub clock_skew_secs: Option<u64>,
    /// The clock, as Unix seconds; for tests. Default: the system clock.
    pub clock: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
}

impl fmt::Debug for VerifyOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyOptions")
            .field("key_set", &self.key_set)
            .field("issuer", &self.issuer)
            .field("project_id", &self.project_id)
            .field("clock_skew_secs", &self.clock_skew_secs)
            .field("clock", &self.clock.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

/// Per-client cache of the key feed.
#[derive(Default)]
pub(crate) struct RampartState {
    feed: Mutex<Option<JwkSet>>,
}

impl fmt::Debug for RampartState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RampartState").finish_non_exhaustive()
    }
}

/// `client.rampart()`.
pub struct RampartNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

fn invalid(message: impl Into<String>) -> KError {
    KError::new("K9O_INVALID_FIELD_VALUE", message)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64(part: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(part).ok()
}

fn find_key<'k>(keys: &'k [Jwk], kid: &str) -> Option<&'k Jwk> {
    keys.iter().find(|k| k.kid.as_deref() == Some(kid))
}

fn public_key_bytes(jwk: &Jwk) -> Option<[u8; 32]> {
    if jwk.kty != "OKP" || jwk.crv.as_deref() != Some("Ed25519") {
        return None;
    }
    let raw = b64(jwk.x.as_deref()?)?;
    <[u8; 32]>::try_from(raw.as_slice()).ok()
}

fn int_claim(claims: &Value, name: &str) -> Result<i64, ReceiptRefusal> {
    match claims.get(name) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.floor() as i64))
            .ok_or(())
            .or_else(|_| {
                refuse(
                    ReceiptRefusalReason::Malformed,
                    format!("claim `{name}` is not a number"),
                )
            }),
        _ => refuse(
            ReceiptRefusalReason::Malformed,
            format!("claim `{name}` is missing"),
        ),
    }
}

fn str_claim<'c>(claims: &'c Value, name: &str) -> Result<&'c str, ReceiptRefusal> {
    match claims.get(name).and_then(Value::as_str) {
        Some(s) => Ok(s),
        None => refuse(
            ReceiptRefusalReason::Malformed,
            format!("claim `{name}` is missing"),
        ),
    }
}

impl RampartNamespace<'_> {
    /// `POST /@<slug>/rampart/classify`: classify `input` (a JSON object). `label` (1 to 128
    /// characters) names the context. Advisory: this returns a verdict or an error and blocks
    /// nothing. A classifier outage is 503 `K9O_RAMPART_CLASSIFIER_UNAVAILABLE`.
    pub async fn classify(
        &self,
        input: &Value,
        label: Option<&str>,
    ) -> Result<Response<Classification>, KError> {
        let slug = require_project(self.config, "rampart.classify")?;
        if !input.is_object() {
            return Err(invalid("rampart.classify: input must be a JSON object"));
        }
        if let Some(l) = label {
            let n = l.chars().count();
            if !(1..=128).contains(&n) {
                return Err(invalid(
                    "rampart.classify: label must be 1 to 128 characters",
                ));
            }
        }
        let mut body = json!({ "input": input });
        if let Some(l) = label {
            body["label"] = Value::String(l.to_string());
        }
        let path = project_path(slug, "rampart/classify");
        request::<Classification, Value>(self.config, Method::POST, &path, Some(&body), None).await
    }

    /// `GET /rampart/jwks`: the public keys that sign classification receipts. Public; the path
    /// has no project prefix. The answer carries `issuer` next to `keys` when the deployment
    /// advertises one. Also refreshes the key cache used by `verify_receipt`.
    pub async fn jwks(&self) -> Result<Response<JwkSet>, KError> {
        let resp = request_bare::<JwkSet>(self.config, Method::GET, "/rampart/jwks").await?;
        *self.config.rampart.feed.lock().unwrap() = Some(resp.data.clone());
        Ok(resp)
    }

    async fn fetch_feed(&self) -> Result<JwkSet, ReceiptRefusal> {
        match self.jwks().await {
            Ok(r) => Ok(r.data),
            Err(e) => refuse(
                ReceiptRefusalReason::KeyFeed,
                format!("the key feed could not be fetched: {}", e.code),
            ),
        }
    }

    /// The key for `kid` and the feed's issuer: from `options.key_set` when given, else from the
    /// cached feed, fetching it when empty and refetching once when the `kid` is unknown.
    async fn key_for(
        &self,
        kid: &str,
        options: &VerifyOptions,
    ) -> Result<(Jwk, Option<String>), ReceiptRefusal> {
        let unknown = || ReceiptRefusal {
            reason: ReceiptRefusalReason::KeyUnknown,
            detail: "no key in the key set has the receipt's kid".into(),
        };
        if let Some(set) = &options.key_set {
            return find_key(&set.keys, kid)
                .cloned()
                .map(|k| (k, set.issuer.clone()))
                .ok_or_else(unknown);
        }
        let cached = self.config.rampart.feed.lock().unwrap().clone();
        let (feed, fetched_now) = match cached {
            Some(f) => (f, false),
            None => (self.fetch_feed().await?, true),
        };
        if let Some(k) = find_key(&feed.keys, kid) {
            return Ok((k.clone(), feed.issuer.clone()));
        }
        if fetched_now {
            return Err(unknown());
        }
        let feed = self.fetch_feed().await?;
        find_key(&feed.keys, kid)
            .cloned()
            .map(|k| (k, feed.issuer.clone()))
            .ok_or_else(unknown)
    }

    /// Verify a classification receipt against the exact `input` that was classified, and return
    /// the verdict read from its verified claims. See the module notes for the order of the
    /// checks. Any failure is a [`ReceiptRefusal`]; there is no partial verdict.
    pub async fn verify_receipt(
        &self,
        source: &(impl ReceiptSource + ?Sized),
        input: &Value,
        options: &VerifyOptions,
    ) -> Result<VerifiedReceipt, ReceiptRefusal> {
        use ReceiptRefusalReason as R;
        let receipt = source.receipt();
        let parts: Vec<&str> = receipt.split('.').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return refuse(R::Malformed, "a receipt has three dot-separated parts");
        }
        let header: Value = b64(parts[0])
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(Value::is_object)
            .ok_or_else(|| ReceiptRefusal {
                reason: R::Malformed,
                detail: "the header is not base64url JSON".into(),
            })?;
        let claims: Value = b64(parts[1])
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(Value::is_object)
            .ok_or_else(|| ReceiptRefusal {
                reason: R::Malformed,
                detail: "the claims are not base64url JSON".into(),
            })?;
        let signature = b64(parts[2]).ok_or_else(|| ReceiptRefusal {
            reason: R::Malformed,
            detail: "the signature is not base64url".into(),
        })?;

        // 1. the key, chosen by the header kid from the key feed
        let kid = match header.get("kid").and_then(Value::as_str) {
            Some(k) => k.to_string(),
            None => return refuse(R::KeyUnknown, "the header names no kid"),
        };
        let (jwk, feed_issuer) = self.key_for(&kid, options).await?;
        let pk = match public_key_bytes(&jwk) {
            Some(pk) => pk,
            None => return refuse(R::KeyUnknown, "the key for that kid is not an Ed25519 key"),
        };
        // 2. alg, 3. typ
        if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
            return refuse(R::Alg, "alg must be EdDSA");
        }
        if header.get("typ").and_then(Value::as_str) != Some(RECEIPT_TYP) {
            return refuse(R::Typ, format!("typ must be {RECEIPT_TYP}"));
        }
        // the signature, before any claim is trusted
        let sig = match <[u8; 64]>::try_from(signature.as_slice()) {
            Ok(s) => s,
            Err(_) => return refuse(R::Signature, "the signature has the wrong length"),
        };
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        if !ed25519::verify(&pk, signing_input.as_bytes(), &sig) {
            return refuse(R::Signature, "the signature does not verify");
        }

        // 4. issuer, project, time window
        let iss = str_claim(&claims, "iss")?;
        let expected_issuer = match options.issuer.clone().or(feed_issuer) {
            Some(i) => i,
            None => {
                return refuse(
                    R::Issuer,
                    "the key feed advertises no issuer and none was given in the options",
                )
            }
        };
        if iss != expected_issuer {
            return refuse(R::Issuer, "iss is not the expected issuer");
        }
        let project_id = str_claim(&claims, "project_id")?;
        let expected_project = match options.project_id.as_deref().or(source.project_id()) {
            Some(p) => p,
            None => {
                return refuse(
                    R::ProjectUnknown,
                    "no expected project: pass the classify result or the project_id option",
                )
            }
        };
        if project_id != expected_project {
            return refuse(R::Project, "project_id is not the expected project");
        }
        let iat = int_claim(&claims, "iat")?;
        let exp = int_claim(&claims, "exp")?;
        let skew = options.clock_skew_secs.unwrap_or(60) as i64;
        let now = options.clock.as_ref().map_or_else(now_secs, |c| c());
        if now - skew >= exp {
            return refuse(R::Expired, "the receipt is past its exp");
        }
        if iat > now + skew {
            return refuse(R::NotYetValid, "iat is in the future");
        }

        // 5. the input digest, recomputed here
        let canonical = match jcs::canonicalize(input) {
            Ok(c) => c,
            Err(e) => {
                return refuse(
                    R::InputHash,
                    format!("the input cannot be canonicalised: {e}"),
                )
            }
        };
        let digest = hex(&sha256(canonical.as_bytes()));
        let claimed = str_claim(&claims, "input_sha256")?;
        if claimed != digest {
            return refuse(R::InputHash, "input_sha256 does not match the input");
        }

        // 6. the verdict, from the verified claims
        let categories = match claims.get("categories").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .map(|c| c.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
                .ok_or_else(|| ReceiptRefusal {
                    reason: R::Malformed,
                    detail: "categories must be strings".into(),
                })?,
            None => return refuse(R::Malformed, "claim `categories` is missing"),
        };
        let windows = match claims.get("windows") {
            None | Some(Value::Null) => 1,
            Some(_) => {
                let n = int_claim(&claims, "windows")?;
                if n < 1 {
                    return refuse(R::Malformed, "claim `windows` must be at least 1");
                }
                n
            }
        };
        Ok(VerifiedReceipt {
            windows,
            classification: str_claim(&claims, "classification")?.to_string(),
            risk_bp: int_claim(&claims, "risk_bp")?,
            confidence_bp: int_claim(&claims, "confidence_bp")?,
            categories,
            calibration_version: str_claim(&claims, "calibration_version")?.to_string(),
            project_id: project_id.to_string(),
            input_sha256: claimed.to_string(),
            issuer: iss.to_string(),
            issued_at: iat,
            expires_at: exp,
            kid,
        })
    }
}
