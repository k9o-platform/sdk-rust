//! Gate namespace — `client.gate.*`.
//!
//! # Locating the end user's region
//!
//! `region.rs` finds the org's regional API host. An end user can live in another region of the project, so password
//! sign-in first asks the project which region holds the address (`POST /@<project>/gate/locate { email }` at that
//! regional base, with no credential of any kind) and sends the password to the origin it names. The answer is the
//! same for a known and an unknown address.
//!
//! Rules:
//!   - The located origin is used for that one attempt. It is never cached as authority. The one thing kept is a
//!     bounded hint (64), per refresh token, so that refresh and sign-out reach the region that issued the session;
//!     a stale hint gets a 401.
//!   - An origin is used only if it is `https` (`http` only when the base is itself `http` on a loopback host and the
//!     origin is loopback too), has no userinfo, port, path, query or fragment, and its host is exactly the base
//!     host's org, family and apex with the region code `[a-z]{2}\d{2}` added or swapped in the service label, and
//!     that code equals the answer's `region`. Anything else, or `origin: null`, means the base is used. A password
//!     never goes to a host that fails this.
//!   - An error from the door (429 `K9O_RATE_LIMITED`, 503 `K9O_GATE_REGION_UNAVAILABLE`, anything else) is returned
//!     as the usual `KError`, the password is not sent, and nothing is retried. A 2xx with a malformed body uses
//!     the base.

use crate::envelope::Response;
use crate::http::{request, request_to, HttpClientConfig, Target};
use crate::kerror::KError;
use crate::url_grammar::{parse_api_host, project_path};
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;
use url::Url;

fn is_loopback(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || h == "127.0.0.1" || h == "[::1]" || h == "::1"
}

fn region_code_ok(region: &str) -> bool {
    let b = region.as_bytes();
    b.len() == 4
        && b[0].is_ascii_lowercase()
        && b[1].is_ascii_lowercase()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
}

/// `scheme://host[:port]` of a parsed URL.
fn origin_of(u: &Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{}://{}:{}", u.scheme(), h, p),
        (Some(h), None) => format!("{}://{}", u.scheme(), h),
        _ => String::new(),
    }
}

/// The origin the credential goes to: the located one if it passes the rule, else `base`. Pure; never panics.
/// `answer` is the `data` of the locate envelope.
pub(crate) fn choose_located_origin(base: &str, answer: &Value) -> String {
    choose(base, answer).unwrap_or_else(|| base.to_string())
}

fn choose(base: &str, answer: &Value) -> Option<String> {
    let region = answer.get("region")?.as_str()?;
    let origin = answer.get("origin")?.as_str()?;
    if !region_code_ok(region) {
        return None;
    }
    let b = Url::parse(base).ok()?;
    let u = Url::parse(origin).ok()?;
    if !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.query().map_or(false, |q| !q.is_empty())
        || u.fragment().map_or(false, |f| !f.is_empty())
        || u.path() != "/"
    {
        return None;
    }
    let bh = b.host_str()?;
    let uh = u.host_str()?;
    let http_loopback =
        u.scheme() == "http" && b.scheme() == "http" && is_loopback(bh) && is_loopback(uh);
    if u.scheme() != "https" && !http_loopback {
        return None;
    }
    if u.scheme() == b.scheme() && uh == bh && u.port() == b.port() {
        return Some(origin_of(&b));
    }
    let from = parse_api_host(bh)?;
    let to = parse_api_host(uh)?;
    if to.region.as_deref() != Some(region) {
        return None;
    }
    if from.org_slug != to.org_slug || from.family != to.family || from.apex != to.apex {
        return None;
    }
    Some(origin_of(&u))
}

const HINT_LIMIT: usize = 64;

/// refresh token -> origin that issued the session (a hint, not authority); oldest first.
static HINTS: Mutex<VecDeque<(String, String)>> = Mutex::new(VecDeque::new());

fn remember_session_origin(refresh_token: Option<&str>, origin: &str) {
    let Some(t) = refresh_token.filter(|t| !t.is_empty()) else {
        return;
    };
    if let Ok(mut h) = HINTS.lock() {
        h.retain(|(k, _)| k != t);
        h.push_back((t.to_string(), origin.to_string()));
        while h.len() > HINT_LIMIT {
            h.pop_front();
        }
    }
}

fn session_origin_hint(refresh_token: &str) -> Option<String> {
    HINTS
        .lock()
        .ok()?
        .iter()
        .find(|(k, _)| k == refresh_token)
        .map(|(_, v)| v.clone())
}

fn forget_session_origin(refresh_token: &str) {
    if let Ok(mut h) = HINTS.lock() {
        h.retain(|(k, _)| k != refresh_token);
    }
}

/// A refresh answers with a new refresh token and the old one stops working: the hint follows the session. Data
/// without a new token changes nothing.
fn rotate_session_origin(old_refresh_token: &str, data: &Value) {
    let next = data.get("refreshToken").and_then(Value::as_str);
    let origin = session_origin_hint(old_refresh_token);
    if let (Some(next), Some(origin)) = (next.filter(|t| !t.is_empty()), origin) {
        forget_session_origin(old_refresh_token);
        remember_session_origin(Some(next), &origin);
    }
}

/// Is `e` the error for a 2xx answer whose body could not be read as an envelope?
fn is_malformed_ok(e: &KError) -> bool {
    e.code == "K9O_INTERNAL_ERROR"
        && e.status.is_none()
        && e.context
            .get("status")
            .and_then(Value::as_u64)
            .map_or(false, |s| (200..300).contains(&s))
}

#[derive(Serialize)]
struct LocateInput<'a> {
    email: &'a str,
}

/// Ask the project which region holds `email` and return the origin the credential goes to.
/// Returns the door's own error (nothing is retried); a 2xx with a malformed body gives the base, and so does a
/// 404 (the project has no locate step).
async fn locate_origin(
    config: &HttpClientConfig,
    slug: &str,
    email: &str,
) -> Result<String, KError> {
    let base = crate::region::resolve_origin(&config.base_url, &config.region).await;
    match request_to::<Value, _>(
        config,
        Method::POST,
        &project_path(slug, "gate/locate"),
        Some(&LocateInput { email }),
        Target::Anonymous,
    )
    .await
    {
        Ok(env) => Ok(choose_located_origin(&base, &env.data)),
        Err(e) if is_malformed_ok(&e) || e.status == Some(404) => Ok(base),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Serialize)]
pub struct SignInInput<'a> {
    pub email: &'a str,
    pub password: &'a str,
}

#[derive(Debug, Serialize)]
pub struct SignUpInput<'a> {
    pub email: &'a str,
    pub password: &'a str,
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct SignOutInput<'a> {
    /// The refresh token of the session to end. Required: a sign-out without
    /// one is a 400.
    #[serde(rename = "refreshToken")]
    pub refresh_token: &'a str,
}

#[derive(Debug, Serialize)]
pub struct RefreshTokenInput<'a> {
    #[serde(rename = "refreshToken")]
    pub refresh_token: &'a str,
}

pub struct GateNamespace<'a> {
    pub(crate) config: &'a HttpClientConfig,
}

impl<'a> GateNamespace<'a> {
    fn require_project(&self) -> Result<&str, KError> {
        self.config.project_slug.as_deref().ok_or_else(|| {
            KError::new(
                "K9O_MISSING_PROJECT_CONTEXT",
                "client.gate operations require project_slug bound on the client",
            )
        })
    }

    pub async fn sign_up(&self, input: SignUpInput<'_>) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        request::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "gate/signup"),
            Some(&input),
            None,
        )
        .await
    }

    /// Sign in with email and password. First asks the project which region holds the email (one extra request
    /// carrying only the email), then sends the password to that region's host if it is a host of your own org and
    /// apex, else to the base URL. A 429 `K9O_RATE_LIMITED` or 503 `K9O_GATE_REGION_UNAVAILABLE` from that first
    /// step is returned as the `KError`, the password is not sent, and it is safe to retry.
    pub async fn sign_in(&self, input: SignInInput<'_>) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        let origin = locate_origin(self.config, slug, input.email).await?;
        let out = request_to::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "gate/signin"),
            Some(&input),
            Target::Origin(&origin),
        )
        .await?;
        remember_session_origin(
            out.data
                .get("session")
                .and_then(|s| s.get("refreshToken"))
                .and_then(Value::as_str),
            &origin,
        );
        Ok(out)
    }

    pub async fn sign_out(&self, input: SignOutInput<'_>) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        let hint = session_origin_hint(input.refresh_token);
        let out = request_to::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "gate/signout"),
            Some(&input),
            hint.as_deref().map_or(Target::Base, Target::Origin),
        )
        .await?;
        forget_session_origin(input.refresh_token);
        Ok(out)
    }

    pub async fn whoami(&self) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        request::<Value, ()>(
            self.config,
            Method::GET,
            &project_path(slug, "gate/whoami"),
            None,
            None,
        )
        .await
    }

    /// End every live session of the signed-in end user, on every device, this one included
    /// (`POST /@<project>/auth/sessions/revoke-all`). Call it with the end user's access token as the client's
    /// `api_key` (the token `whoami` uses), not a refresh token. The answer's `data` is `{ revokedCount }`. Each ended
    /// session's next refresh returns a `KError` with code `K9O_SESSION_EXPIRED` and a `reason` of `"revoked"` in its
    /// `context`; access tokens already issued keep working until they expire. An operator's token returns
    /// `K9O_TRUST_ZONE_VIOLATION` (403), and a token issued for another project `K9O_PROJECT_SCOPE_MISMATCH` (403).
    pub async fn revoke_all_sessions(&self) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        request::<Value, ()>(
            self.config,
            Method::POST,
            &project_path(slug, "auth/sessions/revoke-all"),
            None,
            None,
        )
        .await
    }

    /// Exchange the refresh token for a new access token and a new refresh token. The answer's `data` is
    /// `{ accessToken, refreshToken, expiresAt }`. The refresh token you sent stops working: store the returned
    /// `refreshToken` before the next refresh, and refresh one at a time per session. A second call with a secret
    /// that was already used is a replay: the session is revoked and the call returns a `KError` with code
    /// `K9O_SESSION_EXPIRED` and a `reason` of `"replay"` in its `context`. The newer secret then gets `"revoked"`.
    /// The other reasons are `"inactive"` and `"max_length"`; sign in again.
    pub async fn refresh_token(
        &self,
        input: RefreshTokenInput<'_>,
    ) -> Result<Response<Value>, KError> {
        let slug = self.require_project()?;
        let hint = session_origin_hint(input.refresh_token);
        let out = request_to::<Value, _>(
            self.config,
            Method::POST,
            &project_path(slug, "gate/refresh"),
            Some(&input),
            hint.as_deref().map_or(Target::Base, Target::Origin),
        )
        .await?;
        rotate_session_origin(input.refresh_token, &out.data);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{test_routes, RetryConfig};
    use crate::region::RegionState;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const BASE: &str = "https://acme.api-de01.k9o.io";

    // ---- the pure rule ----

    #[test]
    fn uses_the_located_origin_when_it_passes_the_rule() {
        let ok: [(&str, &str, Value); 3] = [
            (
                "a sibling region of the same org, family and apex",
                "https://acme.api-us01.k9o.io",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io"}),
            ),
            (
                "the base's own region",
                BASE,
                json!({"region": "de01", "origin": BASE}),
            ),
            (
                "a trailing slash",
                "https://acme.api-us01.k9o.io",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io/"}),
            ),
        ];
        for (name, want, answer) in ok {
            assert_eq!(choose_located_origin(BASE, &answer), want, "{name}");
        }
    }

    #[test]
    fn falls_back_to_the_base_for_everything_else() {
        let refused: Vec<(&str, Value)> = vec![
            (
                "null origin (single host, self-hosted)",
                json!({"region": "de01", "origin": null}),
            ),
            ("a missing answer", Value::Null),
            (
                "http",
                json!({"region": "us01", "origin": "http://acme.api-us01.k9o.io"}),
            ),
            (
                "userinfo",
                json!({"region": "us01", "origin": "https://u:p@acme.api-us01.k9o.io"}),
            ),
            (
                "a port",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io:8443"}),
            ),
            (
                "a path",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io/x"}),
            ),
            (
                "a query",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io/?a=1"}),
            ),
            (
                "another org",
                json!({"region": "us01", "origin": "https://evil.api-us01.k9o.io"}),
            ),
            (
                "another apex",
                json!({"region": "us01", "origin": "https://acme.api-us01.evil.io"}),
            ),
            (
                "another family",
                json!({"region": "us01", "origin": "https://acme.api-staging-us01.k9o.io"}),
            ),
            (
                "a code that differs from region",
                json!({"region": "us01", "origin": "https://acme.api-fr01.k9o.io"}),
            ),
            (
                "a region that is not a code",
                json!({"region": "usa", "origin": "https://acme.api-usa.k9o.io"}),
            ),
            (
                "a host with no region",
                json!({"region": "us01", "origin": "https://acme.api.k9o.io"}),
            ),
            (
                "a region as a subdomain",
                json!({"region": "us01", "origin": "https://us01.acme.api-de01.k9o.io"}),
            ),
            (
                "a look-alike apex",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io.evil.io"}),
            ),
            (
                "a non-string origin",
                json!({"region": "us01", "origin": 7}),
            ),
            (
                "a fragment",
                json!({"region": "us01", "origin": "https://acme.api-us01.k9o.io/#x"}),
            ),
        ];
        for (name, answer) in refused {
            assert_eq!(choose_located_origin(BASE, &answer), BASE, "{name}");
        }
    }

    #[test]
    fn allows_http_only_when_the_base_is_http_on_loopback_and_then_only_the_same_host() {
        let same = json!({"region": "de01", "origin": "http://localhost:8080"});
        assert_eq!(
            choose_located_origin("http://localhost:8080", &same),
            "http://localhost:8080"
        );
        let other = json!({"region": "de01", "origin": "http://evil.localhost:8080"});
        assert_eq!(
            choose_located_origin("http://localhost:8080", &other),
            "http://localhost:8080"
        );
        let down = json!({"region": "de01", "origin": "http://localhost"});
        assert_eq!(
            choose_located_origin("https://localhost", &down),
            "https://localhost"
        );
    }

    #[test]
    fn uses_the_base_when_the_base_is_not_a_gateway_host_shape() {
        let a = json!({"region": "us01", "origin": "https://gw-us01.internal.example"});
        assert_eq!(
            choose_located_origin("https://gw.internal.example", &a),
            "https://gw.internal.example"
        );
    }

    // ---- the flow ----

    fn cfg(org: &str) -> HttpClientConfig {
        HttpClientConfig {
            base_url: format!("https://{org}.api-de01.k9o.io"),
            api_key: Some("secret-key".into()),
            org_slug: Some(org.into()),
            project_slug: Some("portal".into()),
            default_headers: HashMap::from([("x-custom".into(), "v".into())]),
            reqwest: Arc::new(reqwest::Client::new()),
            retry: RetryConfig {
                max_retries: 0,
                ..Default::default()
            },
            region: Arc::new(RegionState::new()),
            rampart: Arc::new(crate::rampart::RampartState::default()),
            crypto: Arc::new(Default::default()),
            expected_key_refs: None,
        }
    }

    fn auth(token: &str) -> Value {
        json!({"data": {"user": {"id": "eu_1"}, "session": {"accessToken": "a", "refreshToken": token}}, "meta": {}})
    }

    /// Mounts the door answer and the three credential routes on `srv`.
    async fn serve(srv: &MockServer, door: ResponseTemplate, token: &str) {
        Mock::given(method("POST"))
            .and(path("/@portal/gate/locate"))
            .respond_with(door)
            .mount(srv)
            .await;
        Mock::given(path("/@portal/gate/signin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth(token)))
            .mount(srv)
            .await;
        for p in ["refresh", "signout"] {
            Mock::given(path(format!("/@portal/gate/{p}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": {"revoked": true}, "meta": {}})),
                )
                .mount(srv)
                .await;
        }
    }

    fn door_says(region: &str, origin: Value) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"data": {"region": region, "origin": origin}, "meta": {}}))
    }

    fn paths(reqs: &[Request]) -> Vec<String> {
        reqs.iter().map(|r| r.url.path().to_string()).collect()
    }

    fn input() -> SignInInput<'static> {
        SignInInput {
            email: "a@b.co",
            password: "pw",
        }
    }

    #[tokio::test]
    async fn sign_in_sends_only_the_email_to_the_door_then_the_password_to_the_located_origin() {
        let (base, us) = (MockServer::start().await, MockServer::start().await);
        test_routes::route("https://acme.api-de01.k9o.io", &base.uri());
        test_routes::route("https://acme.api-us01.k9o.io", &us.uri());
        serve(
            &base,
            door_says("us01", json!("https://acme.api-us01.k9o.io")),
            "r-a",
        )
        .await;
        serve(&us, door_says("us01", Value::Null), "r-a").await;
        let c = cfg("acme");
        GateNamespace { config: &c }.sign_in(input()).await.unwrap();

        let b = base.received_requests().await.unwrap();
        assert_eq!(paths(&b), ["/@portal/gate/locate"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&b[0].body).unwrap(),
            json!({"email": "a@b.co"})
        );
        for h in ["authorization", "x-custom", "cookie"] {
            assert!(b[0].headers.get(h).is_none(), "{h} leaked on the door");
        }
        let u = us.received_requests().await.unwrap();
        assert_eq!(paths(&u), ["/@portal/gate/signin"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&u[0].body).unwrap(),
            json!({"email": "a@b.co", "password": "pw"})
        );
        assert!(u[0].headers.get("authorization").is_some());
    }

    #[tokio::test]
    async fn sign_in_never_sends_the_password_to_an_origin_that_fails_the_rule() {
        let base = MockServer::start().await;
        test_routes::route("https://evil1.api-de01.k9o.io", &base.uri());
        serve(
            &base,
            door_says("us01", json!("https://evil.example")),
            "r-b",
        )
        .await;
        let c = cfg("evil1");
        GateNamespace { config: &c }.sign_in(input()).await.unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/locate", "/@portal/gate/signin"]
        );
    }

    #[tokio::test]
    async fn sign_in_uses_the_base_when_the_door_answers_origin_null() {
        let base = MockServer::start().await;
        test_routes::route("https://nullo.api-de01.k9o.io", &base.uri());
        serve(&base, door_says("de01", Value::Null), "r-c").await;
        let c = cfg("nullo");
        GateNamespace { config: &c }.sign_in(input()).await.unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/locate", "/@portal/gate/signin"]
        );
    }

    #[tokio::test]
    async fn sign_in_uses_the_base_when_the_door_answers_2xx_with_a_malformed_body() {
        let base = MockServer::start().await;
        test_routes::route("https://malf.api-de01.k9o.io", &base.uri());
        serve(
            &base,
            ResponseTemplate::new(200).set_body_string("not json"),
            "r-d",
        )
        .await;
        let c = cfg("malf");
        GateNamespace { config: &c }.sign_in(input()).await.unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/locate", "/@portal/gate/signin"]
        );
    }

    fn not_found() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_json(
            json!({"code": "K9O_NOT_FOUND", "message": "x", "context": {}, "recommendation": null}),
        )
    }

    #[tokio::test]
    async fn sign_in_uses_the_base_when_the_door_answers_404() {
        let base = MockServer::start().await;
        test_routes::route("https://gone404.api-de01.k9o.io", &base.uri());
        serve(&base, not_found(), "r-f").await;
        let c = cfg("gone404");
        GateNamespace { config: &c }.sign_in(input()).await.unwrap();
        let reqs = base.received_requests().await.unwrap();
        assert_eq!(
            paths(&reqs),
            ["/@portal/gate/locate", "/@portal/gate/signin"]
        );
        assert_eq!(reqs[1].url.host_str(), reqs[0].url.host_str());
    }

    #[tokio::test]
    async fn sign_in_raises_a_404_on_the_credential_request_after_a_404_door() {
        let base = MockServer::start().await;
        test_routes::route("https://nodoor2.api-de01.k9o.io", &base.uri());
        Mock::given(path("/@portal/gate/locate"))
            .respond_with(not_found())
            .mount(&base)
            .await;
        Mock::given(path("/@portal/gate/signin"))
            .respond_with(not_found())
            .mount(&base)
            .await;
        let c = cfg("nodoor2");
        let e = GateNamespace { config: &c }
            .sign_in(input())
            .await
            .unwrap_err();
        assert_eq!(e.status, Some(404));
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/locate", "/@portal/gate/signin"]
        );
    }

    #[tokio::test]
    async fn sign_in_sends_nothing_when_the_door_is_down_or_rate_limited() {
        for (status, code, org) in [
            (503u16, "K9O_GATE_REGION_UNAVAILABLE", "down"),
            (429, "K9O_RATE_LIMITED", "limited"),
        ] {
            let base = MockServer::start().await;
            test_routes::route(&format!("https://{org}.api-de01.k9o.io"), &base.uri());
            let body = json!({"code": code, "message": "x", "context": {}, "recommendation": {"retryAfterSec": 3}});
            serve(
                &base,
                ResponseTemplate::new(status).set_body_json(body),
                "r-e",
            )
            .await;
            let c = cfg(org);
            let e = GateNamespace { config: &c }
                .sign_in(input())
                .await
                .unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.status, Some(status));
            assert_eq!(
                base.received_requests().await.unwrap().len(),
                1,
                "{code}: the credential must not be sent, nor the door retried"
            );
        }
    }

    #[tokio::test]
    async fn the_located_origin_is_not_cached_the_next_sign_in_asks_again() {
        let (base, us, fr) = (
            MockServer::start().await,
            MockServer::start().await,
            MockServer::start().await,
        );
        test_routes::route("https://nocache.api-de01.k9o.io", &base.uri());
        test_routes::route("https://nocache.api-us01.k9o.io", &us.uri());
        test_routes::route("https://nocache.api-fr01.k9o.io", &fr.uri());
        Mock::given(path("/@portal/gate/locate"))
            .respond_with(door_says("us01", json!("https://nocache.api-us01.k9o.io")))
            .up_to_n_times(1)
            .mount(&base)
            .await;
        Mock::given(path("/@portal/gate/locate"))
            .respond_with(door_says("fr01", json!("https://nocache.api-fr01.k9o.io")))
            .mount(&base)
            .await;
        serve(&us, door_says("us01", Value::Null), "r-f1").await;
        serve(&fr, door_says("fr01", Value::Null), "r-f2").await;
        let c = cfg("nocache");
        let g = GateNamespace { config: &c };
        g.sign_in(input()).await.unwrap();
        g.sign_in(input()).await.unwrap();
        let b = base.received_requests().await.unwrap();
        assert_eq!(paths(&b), ["/@portal/gate/locate", "/@portal/gate/locate"]);
        assert_eq!(
            paths(&us.received_requests().await.unwrap()),
            ["/@portal/gate/signin"]
        );
        assert_eq!(
            paths(&fr.received_requests().await.unwrap()),
            ["/@portal/gate/signin"]
        );
    }

    #[tokio::test]
    async fn refresh_and_sign_out_go_to_the_origin_that_issued_the_session() {
        let (base, us) = (MockServer::start().await, MockServer::start().await);
        test_routes::route("https://hint.api-de01.k9o.io", &base.uri());
        test_routes::route("https://hint.api-us01.k9o.io", &us.uri());
        serve(
            &base,
            door_says("us01", json!("https://hint.api-us01.k9o.io")),
            "r-hint",
        )
        .await;
        serve(&us, door_says("us01", Value::Null), "r-hint").await;
        let c = cfg("hint");
        let g = GateNamespace { config: &c };
        g.sign_in(input()).await.unwrap();
        g.refresh_token(RefreshTokenInput {
            refresh_token: "r-hint",
        })
        .await
        .unwrap();
        g.sign_out(SignOutInput {
            refresh_token: "r-hint",
        })
        .await
        .unwrap();
        assert_eq!(
            paths(&us.received_requests().await.unwrap()),
            [
                "/@portal/gate/signin",
                "/@portal/gate/refresh",
                "/@portal/gate/signout"
            ]
        );
        // The hint is gone after sign-out: the next refresh goes to the base.
        g.refresh_token(RefreshTokenInput {
            refresh_token: "r-hint",
        })
        .await
        .unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/locate", "/@portal/gate/refresh"]
        );
    }

    #[tokio::test]
    async fn refresh_rotates_the_refresh_token_and_the_hint_follows_it() {
        let (base, us) = (MockServer::start().await, MockServer::start().await);
        test_routes::route("https://rot.api-de01.k9o.io", &base.uri());
        test_routes::route("https://rot.api-us01.k9o.io", &us.uri());
        serve(
            &base,
            door_says("us01", json!("https://rot.api-us01.k9o.io")),
            "rot-1",
        )
        .await;
        Mock::given(path("/@portal/gate/signin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(auth("rot-1")))
            .mount(&us)
            .await;
        Mock::given(path("/@portal/gate/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": {"accessToken": "a2", "refreshToken": "rot-2", "expiresAt": "x"}, "meta": {}}),
            ))
            .mount(&us)
            .await;
        Mock::given(path("/@portal/gate/signout"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"revoked": false}, "meta": {}})),
            )
            .mount(&base)
            .await;
        let c = cfg("rot");
        let g = GateNamespace { config: &c };
        g.sign_in(input()).await.unwrap();
        let out = g
            .refresh_token(RefreshTokenInput {
                refresh_token: "rot-1",
            })
            .await
            .unwrap();
        assert_eq!(out.data["refreshToken"], "rot-2");
        g.refresh_token(RefreshTokenInput {
            refresh_token: "rot-2",
        })
        .await
        .unwrap();
        assert_eq!(
            paths(&us.received_requests().await.unwrap()),
            [
                "/@portal/gate/signin",
                "/@portal/gate/refresh",
                "/@portal/gate/refresh"
            ]
        );
        // The old secret has no hint any more: its sign-out goes to the base.
        g.sign_out(SignOutInput {
            refresh_token: "rot-1",
        })
        .await
        .unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap())
                .last()
                .unwrap(),
            "/@portal/gate/signout"
        );
    }

    #[tokio::test]
    async fn a_refused_refresh_carries_the_session_expired_reason() {
        for reason in ["replay", "revoked", "inactive", "max_length"] {
            let base = MockServer::start().await;
            test_routes::route("https://expired.api-de01.k9o.io", &base.uri());
            Mock::given(path("/@portal/gate/refresh"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "code": "K9O_SESSION_EXPIRED",
                    "message": "The session has ended.",
                    "context": {"reason": reason}
                })))
                .mount(&base)
                .await;
            let c = cfg("expired");
            let g = GateNamespace { config: &c };
            let e = g
                .refresh_token(RefreshTokenInput {
                    refresh_token: "exp-1",
                })
                .await
                .unwrap_err();
            assert_eq!(e.code, "K9O_SESSION_EXPIRED");
            assert_eq!(e.status, Some(401));
            assert_eq!(e.context["reason"], reason);
        }
    }

    #[tokio::test]
    async fn revoke_all_sessions_posts_with_the_bearer_and_returns_the_count() {
        let base = MockServer::start().await;
        test_routes::route("https://revokeall.api-de01.k9o.io", &base.uri());
        Mock::given(path("/@portal/auth/sessions/revoke-all"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"revokedCount": 3}, "meta": {}})),
            )
            .mount(&base)
            .await;
        let c = cfg("revokeall");
        let g = GateNamespace { config: &c };
        let out = g.revoke_all_sessions().await.unwrap();
        assert_eq!(out.data["revokedCount"], 3);
        let seen = base.received_requests().await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method.as_str(), "POST");
        assert_eq!(seen[0].url.path(), "/@portal/auth/sessions/revoke-all");
        assert!(seen[0].body.is_empty());
        assert_eq!(
            seen[0]
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer secret-key"
        );
    }

    #[tokio::test]
    async fn revoke_all_sessions_refusals_are_kerrors() {
        for code in ["K9O_TRUST_ZONE_VIOLATION", "K9O_PROJECT_SCOPE_MISMATCH"] {
            let base = MockServer::start().await;
            test_routes::route("https://revokeallno.api-de01.k9o.io", &base.uri());
            Mock::given(path("/@portal/auth/sessions/revoke-all"))
                .respond_with(
                    ResponseTemplate::new(403)
                        .set_body_json(json!({"code": code, "message": "refused", "context": {}})),
                )
                .mount(&base)
                .await;
            let c = cfg("revokeallno");
            let g = GateNamespace { config: &c };
            let e = g.revoke_all_sessions().await.unwrap_err();
            assert_eq!(e.code, code);
            assert_eq!(e.status, Some(403));
        }
    }

    #[tokio::test]
    async fn sign_up_and_whoami_do_not_use_the_door() {
        let base = MockServer::start().await;
        test_routes::route("https://nodoor.api-de01.k9o.io", &base.uri());
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(auth("r-g")))
            .mount(&base)
            .await;
        let c = cfg("nodoor");
        let g = GateNamespace { config: &c };
        g.sign_up(SignUpInput {
            email: "a@b.co",
            password: "pw-long-enough-1",
            display_name: None,
        })
        .await
        .unwrap();
        g.whoami().await.unwrap();
        assert_eq!(
            paths(&base.received_requests().await.unwrap()),
            ["/@portal/gate/signup", "/@portal/gate/whoami"]
        );
    }

    #[test]
    fn the_hint_map_is_bounded() {
        for i in 0..(HINT_LIMIT + 10) {
            remember_session_origin(Some(&format!("bound-{i}")), "https://o.example");
        }
        assert!(session_origin_hint("bound-0").is_none());
        assert!(session_origin_hint(&format!("bound-{}", HINT_LIMIT + 9)).is_some());
        assert!(HINTS.lock().unwrap().len() <= HINT_LIMIT);
    }
}
