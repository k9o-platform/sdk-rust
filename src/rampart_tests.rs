//! Tests for `client.rampart()`: wire, key feed, and every receipt refusal. Receipts are signed
//! here with a fresh Ed25519 key, so no gateway is involved.

use crate::ed25519::testing::{public_key, sign};
use crate::hash::{hex, sha256};
use crate::rampart::{ReceiptRefusalReason as R, VerifyOptions, RECEIPT_TYP};
use crate::{jcs, Client, ClientOpts, Jwk, JwkSet, RetryConfig};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ISS: &str = "https://api.example.test/rampart";
const PROJECT_ID: &str = "proj_0123456789abcdef";
const NOW: i64 = 1_800_000_000;

fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}

fn jwk(kid: &str, s: &[u8; 32]) -> Jwk {
    Jwk {
        kty: "OKP".into(),
        crv: Some("Ed25519".into()),
        x: Some(B64.encode(public_key(s))),
        kid: Some(kid.into()),
        alg: Some("EdDSA".into()),
    }
}

fn key_set(kid: &str, s: &[u8; 32]) -> JwkSet {
    JwkSet {
        keys: vec![jwk(kid, s)],
        issuer: Some(ISS.into()),
    }
}

fn input() -> Value {
    json!({"text": "hello", "n": 3})
}

fn claims_for(input: &Value) -> Value {
    json!({
        "iss": ISS,
        "iat": NOW - 10,
        "exp": NOW + 3590,
        "input_sha256": hex(&sha256(jcs::canonicalize(input).unwrap().as_bytes())),
        "classification": "suspicious",
        "risk_bp": 6200,
        "confidence_bp": 8100,
        "categories": ["prompt_injection"],
        "project_id": PROJECT_ID,
        "calibration_version": "cal-2026-10",
    })
}

fn header(kid: &str) -> Value {
    json!({"alg": "EdDSA", "kid": kid, "typ": RECEIPT_TYP})
}

fn make(header: &Value, claims: &Value, s: &[u8; 32]) -> String {
    let h = B64.encode(serde_json::to_vec(header).unwrap());
    let c = B64.encode(serde_json::to_vec(claims).unwrap());
    let sig = sign(s, format!("{h}.{c}").as_bytes());
    format!("{h}.{c}.{}", B64.encode(sig))
}

fn opts(ks: JwkSet) -> VerifyOptions {
    VerifyOptions {
        key_set: Some(ks),
        project_id: Some(PROJECT_ID.into()),
        clock: Some(Arc::new(|| NOW)),
        ..Default::default()
    }
}

fn client(server: &MockServer, project: Option<&str>) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: project.map(Into::into),
        retry_config: Some(RetryConfig {
            max_retries: 0,
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap()
}

async fn requests(server: &MockServer) -> Vec<(String, String, Vec<u8>)> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.method.to_string(), r.url.path().to_string(), r.body))
        .collect()
}

// ---- classify / jwks wire ----------------------------------------------------------------------

#[tokio::test]
async fn classify_sends_input_and_label_and_returns_verdict_with_receipt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@portal/rampart/classify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "classification": "suspicious", "riskScore": 6200, "confidence": 8100,
                "categories": ["prompt_injection"], "calibrationVersion": "cal-2026-10",
                "classificationReceipt": "aaa.bbb.ccc", "projectId": "proj_0123456789abcdef"
            },
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let r = c
        .rampart()
        .classify(&json!({"text": "hello"}), Some("support-chat"))
        .await
        .unwrap()
        .data;
    assert_eq!(r.classification, "suspicious");
    assert_eq!(r.risk_score, 6200);
    assert_eq!(r.confidence, 8100);
    assert_eq!(r.categories, vec!["prompt_injection"]);
    assert_eq!(r.calibration_version, "cal-2026-10");
    assert_eq!(r.classification_receipt, "aaa.bbb.ccc");
    assert_eq!(r.project_id.as_deref(), Some("proj_0123456789abcdef"));
    let reqs = requests(&server).await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "POST");
    let body: Value = serde_json::from_slice(&reqs[0].2).unwrap();
    assert_eq!(
        body,
        json!({"input": {"text": "hello"}, "label": "support-chat"})
    );
}

#[tokio::test]
async fn classify_without_label_omits_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"classification": "clear", "riskScore": 0, "confidence": 10000, "categories": [],
                     "calibrationVersion": "v", "classificationReceipt": "a.b.c"}, "meta": {}})))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    c.rampart().classify(&json!({"a": 1}), None).await.unwrap();
    let body: Value = serde_json::from_slice(&requests(&server).await[0].2).unwrap();
    assert_eq!(body, json!({"input": {"a": 1}}));
}

#[tokio::test]
async fn classify_503_is_an_error_never_a_verdict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "code": "K9O_RAMPART_CLASSIFIER_UNAVAILABLE",
            "message": "Rampart classifier unavailable: no_endpoint_configured",
            "context": {"cause": "no_endpoint_configured"}, "recommendation": null
        })))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let err = c
        .rampart()
        .classify(&json!({"text": "hello"}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_RAMPART_CLASSIFIER_UNAVAILABLE");
    assert_eq!(err.status, Some(503));
    assert_eq!(requests(&server).await.len(), 1, "a POST is not retried");
}

#[tokio::test]
async fn classify_checks_before_sending() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let e = c
        .rampart()
        .classify(&json!("text"), None)
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    for bad in [json!(null), json!(7), json!(["a"]), json!(true)] {
        let e = c.rampart().classify(&bad, None).await.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    }
    let e = c
        .rampart()
        .classify(&json!({}), Some(""))
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    let long = "x".repeat(129);
    let e = c
        .rampart()
        .classify(&json!({}), Some(&long))
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    let none = client(&server, None);
    let e = none.rampart().classify(&json!({}), None).await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn jwks_gets_the_public_feed_without_a_project_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{"crv": "Ed25519", "x": "iTSk2O0Hs20ANT0LHI8Q6pBRAM2iYI0I3qtlbqNF1d4", "kty": "OKP",
                      "kid": "rampart-classify-20261012", "alg": "EdDSA", "use": "sig",
                      "k9o_signer_role": "public"}],
            "issuer": "https://api.example.test/rampart",
            "meta": {"context": {"requestId": "req_1"}}
        })))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let r = c.rampart().jwks().await.unwrap();
    assert_eq!(r.data.keys.len(), 1);
    assert_eq!(
        r.data.keys[0].kid.as_deref(),
        Some("rampart-classify-20261012")
    );
    assert_eq!(r.data.keys[0].kty, "OKP");
    assert_eq!(r.data.issuer.as_deref(), Some(ISS));
    assert_eq!(r.data.keys[0].crv.as_deref(), Some("Ed25519"));
    assert_eq!(r.meta.context.unwrap().request_id, "req_1");
    let reqs = requests(&server).await;
    assert_eq!(
        (reqs[0].0.as_str(), reqs[0].1.as_str()),
        ("GET", "/rampart/jwks")
    );
}

// ---- verify_receipt: happy path and every refusal ---------------------------------------------

#[tokio::test]
async fn verifies_a_good_receipt_and_reads_the_verdict_from_the_claims() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    let receipt = make(&header("k1"), &claims_for(&input()), &s);
    let v = c
        .rampart()
        .verify_receipt(&receipt, &input(), &opts(key_set("k1", &s)))
        .await
        .unwrap();
    assert_eq!(v.classification, "suspicious");
    assert_eq!(v.risk_bp, 6200);
    assert_eq!(v.confidence_bp, 8100);
    assert_eq!(v.categories, vec!["prompt_injection"]);
    assert_eq!(v.calibration_version, "cal-2026-10");
    assert_eq!(v.project_id, PROJECT_ID);
    assert_eq!(v.kid, "k1");
    assert_eq!(v.issuer, ISS);
    assert_eq!(v.windows, 1, "a receipt without the claim reads 1 window");
    assert!(
        requests(&server).await.is_empty(),
        "an injected key set means no fetch"
    );
}

async fn refusal(receipt: &str, input: &Value, o: &VerifyOptions) -> crate::ReceiptRefusal {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    c.rampart()
        .verify_receipt(receipt, input, o)
        .await
        .expect_err("must be refused")
}

#[tokio::test]
async fn every_refusal_reason() {
    let s = seed(5);
    let ks = key_set("k1", &s);
    let i = input();
    let good_claims = claims_for(&i);

    // unknown kid
    let r = make(&header("other"), &good_claims, &s);
    assert_eq!(
        refusal(&r, &i, &opts(ks.clone())).await.reason,
        R::KeyUnknown
    );

    // key signed by someone else: kid resolves, signature fails
    let r = make(&header("k1"), &good_claims, &seed(6));
    assert_eq!(
        refusal(&r, &i, &opts(ks.clone())).await.reason,
        R::Signature
    );

    // alg
    let mut h = header("k1");
    h["alg"] = json!("HS256");
    let r = make(&h, &good_claims, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Alg);
    h["alg"] = json!("none");
    let r = make(&h, &good_claims, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Alg);

    // typ
    let mut h = header("k1");
    h["typ"] = json!("JWT");
    let r = make(&h, &good_claims, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Typ);

    // issuer
    let mut c = good_claims.clone();
    c["iss"] = json!("https://elsewhere.example/rampart");
    let r = make(&header("k1"), &c, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Issuer);

    // project
    let mut c = good_claims.clone();
    c["project_id"] = json!("proj_ffffffffffffffff");
    let r = make(&header("k1"), &c, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Project);

    // expired (past exp plus the 60 s skew)
    let mut c = good_claims.clone();
    c["exp"] = json!(NOW - 61);
    let r = make(&header("k1"), &c, &s);
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Expired);

    // not yet valid
    let mut c = good_claims.clone();
    c["iat"] = json!(NOW + 61);
    let r = make(&header("k1"), &c, &s);
    assert_eq!(
        refusal(&r, &i, &opts(ks.clone())).await.reason,
        R::NotYetValid
    );

    // input hash: a different input than the one classified
    let r = make(&header("k1"), &good_claims, &s);
    let other = json!({"text": "goodbye"});
    assert_eq!(
        refusal(&r, &other, &opts(ks.clone())).await.reason,
        R::InputHash
    );

    // tampered signature
    let r = make(&header("k1"), &good_claims, &s);
    let (head, sig) = r.rsplit_once('.').unwrap();
    let mut sig_bytes = B64.decode(sig).unwrap();
    sig_bytes[3] ^= 0x01;
    let bad = format!("{head}.{}", B64.encode(sig_bytes));
    assert_eq!(
        refusal(&bad, &i, &opts(ks.clone())).await.reason,
        R::Signature
    );

    // truncated signature
    let short = format!("{head}.{}", B64.encode([0u8; 10]));
    assert_eq!(
        refusal(&short, &i, &opts(ks.clone())).await.reason,
        R::Signature
    );

    // tampered claims under the original signature
    let parts: Vec<&str> = r.split('.').collect();
    let mut forged = good_claims.clone();
    forged["classification"] = json!("clear");
    let forged = format!(
        "{}.{}.{}",
        parts[0],
        B64.encode(serde_json::to_vec(&forged).unwrap()),
        parts[2]
    );
    assert_eq!(
        refusal(&forged, &i, &opts(ks.clone())).await.reason,
        R::Signature
    );

    // malformed
    for m in ["", "a.b", "a.b.c.d", "!!.!!.!!", "e30.e30."] {
        assert_eq!(
            refusal(m, &i, &opts(ks.clone())).await.reason,
            R::Malformed,
            "{m}"
        );
    }
    // missing claim, validly signed
    let mut c = good_claims.clone();
    c.as_object_mut().unwrap().remove("categories");
    let r = make(&header("k1"), &c, &s);
    assert_eq!(
        refusal(&r, &i, &opts(ks.clone())).await.reason,
        R::Malformed
    );

    // a key that is not an Ed25519 key
    let mut bad_key = ks.clone();
    bad_key.keys[0].crv = Some("P-256".into());
    let r = make(&header("k1"), &good_claims, &s);
    assert_eq!(refusal(&r, &i, &opts(bad_key)).await.reason, R::KeyUnknown);
}

#[tokio::test]
async fn skew_window_is_inclusive_of_60_seconds_and_configurable() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    let i = input();
    let mut cl = claims_for(&i);
    cl["exp"] = json!(NOW - 59);
    let r = make(&header("k1"), &cl, &s);
    assert!(c
        .rampart()
        .verify_receipt(&r, &i, &opts(key_set("k1", &s)))
        .await
        .is_ok());
    let mut strict = opts(key_set("k1", &s));
    strict.clock_skew_secs = Some(0);
    let e = c
        .rampart()
        .verify_receipt(&r, &i, &strict)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::Expired);
}

#[tokio::test]
async fn input_hash_uses_the_canonical_form_not_key_order() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    let classified: Value = serde_json::from_str(r#"{"b": [1, 2], "a": "x"}"#).unwrap();
    let r = make(&header("k1"), &claims_for(&classified), &s);
    let presented: Value = serde_json::from_str(r#"{"a":"x","b":[1,2]}"#).unwrap();
    assert!(c
        .rampart()
        .verify_receipt(&r, &presented, &opts(key_set("k1", &s)))
        .await
        .is_ok());
}

// ---- key feed -----------------------------------------------------------------------------------

fn feed(keys: &[Jwk]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"keys": keys, "issuer": ISS, "meta": {}}))
}

fn feed_options() -> VerifyOptions {
    VerifyOptions {
        key_set: None,
        project_id: Some(PROJECT_ID.into()),
        clock: Some(Arc::new(|| NOW)),
        ..Default::default()
    }
}

async fn feed_requests(server: &MockServer) -> usize {
    requests(server)
        .await
        .iter()
        .filter(|r| r.1 == "/rampart/jwks")
        .count()
}

#[tokio::test]
async fn fetches_the_feed_itself_and_caches_it_for_the_clients_life() {
    let server = MockServer::start().await;
    let s = seed(5);
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(feed(&[jwk("k1", &s)]))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let i = input();
    let r = make(&header("k1"), &claims_for(&i), &s);
    for _ in 0..3 {
        c.rampart()
            .verify_receipt(&r, &i, &feed_options())
            .await
            .unwrap();
    }
    assert_eq!(feed_requests(&server).await, 1);
}

#[tokio::test]
async fn unknown_kid_refetches_once_and_then_finds_the_rotated_key() {
    let server = MockServer::start().await;
    let (old, new) = (seed(5), seed(6));
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(feed(&[jwk("old", &old)]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(feed(&[jwk("old", &old), jwk("new", &new)]))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let i = input();
    let r_old = make(&header("old"), &claims_for(&i), &old);
    let r_new = make(&header("new"), &claims_for(&i), &new);
    c.rampart()
        .verify_receipt(&r_old, &i, &feed_options())
        .await
        .unwrap();
    assert_eq!(feed_requests(&server).await, 1);
    c.rampart()
        .verify_receipt(&r_new, &i, &feed_options())
        .await
        .unwrap();
    assert_eq!(feed_requests(&server).await, 2);
    // now cached: neither receipt fetches again
    c.rampart()
        .verify_receipt(&r_old, &i, &feed_options())
        .await
        .unwrap();
    c.rampart()
        .verify_receipt(&r_new, &i, &feed_options())
        .await
        .unwrap();
    assert_eq!(feed_requests(&server).await, 2);
}

#[tokio::test]
async fn a_kid_that_stays_unknown_costs_one_refetch_per_verification_not_a_loop() {
    let server = MockServer::start().await;
    let s = seed(5);
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(feed(&[jwk("k1", &s)]))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let i = input();
    let stranger = make(&header("nope"), &claims_for(&i), &s);
    // first call: the cache is empty, so the one fetch is the fetch
    let e = c
        .rampart()
        .verify_receipt(&stranger, &i, &feed_options())
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::KeyUnknown);
    assert_eq!(feed_requests(&server).await, 1);
    // second call: cached, unknown kid, exactly one refetch
    let e = c
        .rampart()
        .verify_receipt(&stranger, &i, &feed_options())
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::KeyUnknown);
    assert_eq!(feed_requests(&server).await, 2);
}

#[tokio::test]
async fn an_unreachable_feed_is_a_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "code": "K9O_UPSTREAM_UNAVAILABLE", "message": "down"})))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let i = input();
    let r = make(&header("k1"), &claims_for(&i), &seed(5));
    let e = c
        .rampart()
        .verify_receipt(&r, &i, &feed_options())
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::KeyFeed);
}

#[tokio::test]
async fn jwks_call_primes_the_cache_for_verify() {
    let server = MockServer::start().await;
    let s = seed(5);
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(feed(&[jwk("k1", &s)]))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    c.rampart().jwks().await.unwrap();
    let i = input();
    let r = make(&header("k1"), &claims_for(&i), &s);
    c.rampart()
        .verify_receipt(&r, &i, &feed_options())
        .await
        .unwrap();
    assert_eq!(feed_requests(&server).await, 1);
}

// ---- issuer, project and canonical-form rules ---------------------------------------------------

#[tokio::test]
async fn issuer_comes_from_the_feed_and_the_option_overrides_it() {
    let server = MockServer::start().await;
    let s = seed(5);
    let i = input();
    let c = client(&server, Some("portal"));
    // the feed's issuer is the one checked; the client's base URL plays no part
    let r = make(&header("k1"), &claims_for(&i), &s);
    c.rampart()
        .verify_receipt(&r, &i, &opts(key_set("k1", &s)))
        .await
        .unwrap();
    // a different feed issuer refuses the receipt
    let mut ks = key_set("k1", &s);
    ks.issuer = Some("https://elsewhere.example/rampart".into());
    assert_eq!(refusal(&r, &i, &opts(ks.clone())).await.reason, R::Issuer);
    // the option wins over the feed, both ways
    let mut o = opts(ks.clone());
    o.issuer = Some(ISS.into());
    c.rampart().verify_receipt(&r, &i, &o).await.unwrap();
    let mut o = opts(key_set("k1", &s));
    o.issuer = Some("https://elsewhere.example/rampart".into());
    assert_eq!(refusal(&r, &i, &o).await.reason, R::Issuer);
}

#[tokio::test]
async fn a_feed_without_an_issuer_and_no_option_is_refused() {
    let s = seed(5);
    let i = input();
    let r = make(&header("k1"), &claims_for(&i), &s);
    let mut ks = key_set("k1", &s);
    ks.issuer = None;
    let e = refusal(&r, &i, &opts(ks.clone())).await;
    assert_eq!(e.reason, R::Issuer);
    assert!(e.detail.contains("advertises no issuer"), "{}", e.detail);
    // the same over a fetched feed that has no issuer member
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rampart/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [jwk("k1", &s)], "meta": {}})))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let e = c
        .rampart()
        .verify_receipt(&r, &i, &feed_options())
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::Issuer);
    // with the option it passes
    let mut o = feed_options();
    o.issuer = Some(ISS.into());
    c.rampart().verify_receipt(&r, &i, &o).await.unwrap();
}

fn classified(receipt: &str, project: Option<&str>) -> crate::Classification {
    crate::Classification {
        classification: "suspicious".into(),
        risk_score: 6200,
        confidence: 8100,
        categories: vec!["prompt_injection".into()],
        calibration_version: "cal-2026-10".into(),
        classification_receipt: receipt.into(),
        project_id: project.map(Into::into),
    }
}

#[tokio::test]
async fn project_comes_from_the_classify_result_or_the_option() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    let i = input();
    let receipt = make(&header("k1"), &claims_for(&i), &s);
    let mut o = opts(key_set("k1", &s));
    o.project_id = None;

    // the classify result object carries the receipt and its project id
    let result = classified(&receipt, Some(PROJECT_ID));
    let v = c.rampart().verify_receipt(&result, &i, &o).await.unwrap();
    assert_eq!(v.project_id, PROJECT_ID);

    // a bare receipt string with no project id anywhere is refused as unknown
    let e = c
        .rampart()
        .verify_receipt(&receipt, &i, &o)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::ProjectUnknown);
    assert_eq!(e.reason.as_str(), "project_unknown");
    let e = c
        .rampart()
        .verify_receipt(receipt.as_str(), &i, &o)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::ProjectUnknown);
    let e = c
        .rampart()
        .verify_receipt(&classified(&receipt, None), &i, &o)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::ProjectUnknown);

    // the option makes a bare receipt verifiable, and wins over the result's project id
    o.project_id = Some(PROJECT_ID.into());
    c.rampart().verify_receipt(&receipt, &i, &o).await.unwrap();
    let other = classified(&receipt, Some("proj_ffffffffffffffff"));
    c.rampart().verify_receipt(&other, &i, &o).await.unwrap();
    o.project_id = Some("proj_ffffffffffffffff".into());
    let e = c
        .rampart()
        .verify_receipt(&result, &i, &o)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::Project);

    // a receipt for another project than the result's is a mismatch, not unknown
    o.project_id = None;
    let e = c
        .rampart()
        .verify_receipt(&other, &i, &o)
        .await
        .unwrap_err();
    assert_eq!(e.reason, R::Project);

    // no project lookup is ever made
    assert!(requests(&server).await.is_empty());
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    input: Value,
    #[serde(rename = "expectCanonical")]
    expect_canonical: Option<String>,
    #[serde(rename = "expectError")]
    expect_error: Option<String>,
}

fn cases() -> Vec<Case> {
    #[derive(serde::Deserialize)]
    struct File {
        cases: Vec<Case>,
    }
    let text = include_str!("../tests/fixtures/jcs-codepoint-cases.json");
    serde_json::from_str::<File>(text).unwrap().cases
}

#[test]
fn every_shared_canonical_form_case_matches() {
    let all = cases();
    assert_eq!(all.len(), 25);
    for case in all {
        let got = jcs::canonicalize(&case.input);
        match (&case.expect_canonical, &case.expect_error) {
            (Some(want), None) => assert_eq!(got.as_deref(), Ok(want.as_str()), "{}", case.name),
            (None, Some(kind)) => {
                let want = match kind.as_str() {
                    "numeric_ceiling_exceeded" => jcs::JcsError::NumericCeilingExceeded,
                    "non_integer_number" => jcs::JcsError::NonIntegerNumber,
                    other => panic!("{}: unknown expected error {other}", case.name),
                };
                assert_eq!(got, Err(want), "{}", case.name);
            }
            _ => panic!("{}: needs exactly one expectation", case.name),
        }
    }
}

#[tokio::test]
async fn an_input_that_cannot_be_canonicalised_is_refused_as_input_hash() {
    let s = seed(5);
    let ks = key_set("k1", &s);
    let good = input();
    let r = make(&header("k1"), &claims_for(&good), &s);
    for bad in [
        json!({"text": "hello", "n": 1.5}),
        json!({"big": 9007199254740992u64}),
    ] {
        let e = refusal(&r, &bad, &opts(ks.clone())).await;
        assert_eq!(e.reason, R::InputHash);
        assert!(e.detail.contains("cannot be canonicalised"), "{}", e.detail);
    }
}

#[tokio::test]
async fn classify_does_not_pre_validate_the_input_canonical_form() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "K9O_INVALID_FIELD_VALUE", "message": "input is not canonicalisable"})))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let e = c
        .rampart()
        .classify(&json!({"x": 1.5}), None)
        .await
        .unwrap_err();
    assert_eq!(e.status, Some(400));
    assert_eq!(requests(&server).await.len(), 1, "the request was sent");
}

// ---- windows claim and the 413 answer ---------------------------------------------------------

#[tokio::test]
async fn windows_claim_is_exposed_and_unknown_claims_are_tolerated() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    let mut claims = claims_for(&input());
    claims["windows"] = json!(4);
    claims["some_future_claim"] = json!({"a": [1, 2]});
    let receipt = make(&header("k1"), &claims, &s);
    let v = c
        .rampart()
        .verify_receipt(&receipt, &input(), &opts(key_set("k1", &s)))
        .await
        .unwrap();
    assert_eq!(v.windows, 4);
    assert_eq!(v.classification, "suspicious");
}

#[tokio::test]
async fn a_windows_claim_that_is_not_a_positive_number_is_malformed() {
    let server = MockServer::start().await;
    let c = client(&server, Some("portal"));
    let s = seed(5);
    for bad in [json!("4"), json!(0), json!(-2), json!(true)] {
        let mut claims = claims_for(&input());
        claims["windows"] = bad;
        let receipt = make(&header("k1"), &claims, &s);
        let err = c
            .rampart()
            .verify_receipt(&receipt, &input(), &opts(key_set("k1", &s)))
            .await
            .unwrap_err();
        assert_eq!(err.reason, R::Malformed);
    }
}

#[tokio::test]
async fn classify_413_input_too_long_is_a_typed_error_never_a_verdict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "code": "K9O_RAMPART_INPUT_TOO_LONG",
            "message": "The input needs more than 16 windows",
            "context": {"reason": "rampart_input_too_long"}, "recommendation": null
        })))
        .mount(&server)
        .await;
    let c = client(&server, Some("portal"));
    let err = c
        .rampart()
        .classify(&json!({"text": "long"}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_RAMPART_INPUT_TOO_LONG");
    assert_eq!(err.status, Some(413));
    assert_eq!(err.context["reason"], "rampart_input_too_long");
    assert_eq!(requests(&server).await.len(), 1, "a 413 is not retried");
}
