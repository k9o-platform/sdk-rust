//! Key inventory: `keys().list / get / rotate` over a stubbed transport.

use k9o::{
    Client, ClientOpts, KError, KeyStatusFilter, RetryConfig, RotateOptions, StepUpChallenge,
    StepUpProof,
};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(t: ResponseTemplate) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(t).mount(&server).await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        retry_config: Some(RetryConfig {
            max_retries: 0,
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

fn json_reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn key(id: &str, version: i64, status: &str) -> Value {
    json!({"id": id, "version": version, "status": status, "custodyMode": "PLATFORM_MANAGED",
           "createdAt": "2026-10-01T10:00:00.000Z", "rotatedAt": null, "byokRef": null, "hyokRef": null})
}

#[tokio::test]
async fn list_defaults_to_the_active_key() {
    let (server, c) = setup(json_reply(
        200,
        json!({"data": {"items": [key("vkey_1", 2, "active")], "nextCursor": null}, "meta": {}}),
    ))
    .await;
    let l = c.keys().list(None).await.unwrap().data;
    assert_eq!(l.items.len(), 1);
    let k = &l.items[0];
    assert_eq!(k.id, "vkey_1");
    assert_eq!(k.version, Some(2));
    assert_eq!(k.status.as_deref(), Some("active"));
    assert_eq!(k.custody_mode.as_deref(), Some("PLATFORM_MANAGED"));
    assert_eq!(k.created_at.as_deref(), Some("2026-10-01T10:00:00.000Z"));
    assert!(k.rotated_at.is_none() && k.byok_ref.is_none() && k.hyok_ref.is_none());
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(req.method.as_str(), "GET");
    assert_eq!(req.url.path(), "/@portal/vault/keys");
    assert!(req.url.query().is_none());
}

#[tokio::test]
async fn list_all_sends_status_all() {
    let (server, c) = setup(json_reply(
        200,
        json!({"data": {"items": [key("vkey_2", 2, "active"), key("vkey_1", 1, "revoked")], "nextCursor": "c2"}, "meta": {}}),
    ))
    .await;
    let l = c
        .keys()
        .list(Some(KeyStatusFilter::All))
        .await
        .unwrap()
        .data;
    assert_eq!(l.items.len(), 2);
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(req.url.query(), Some("status=all"));

    c.keys().list(Some(KeyStatusFilter::Active)).await.unwrap();
    let all = server.received_requests().await.unwrap();
    assert!(all[1].url.query().is_none());
}

#[tokio::test]
async fn key_fields_byok_and_hyok() {
    let (server, c) = setup(json_reply(
        200,
        json!({"data": {"id": "vkey_9", "version": 3, "status": "rotating", "custodyMode": "BYOK",
                         "createdAt": "2026-10-01T10:00:00.000Z", "rotatedAt": "2026-10-20T08:00:00.000Z",
                         "byokRef": {"provider": "aws-kms", "region": "eu-central-1", "externalKeyFingerprint": "AbCdEfGh01234567"},
                         "hyokRef": null}, "meta": {}}),
    ))
    .await;
    let k = c.keys().get("vkey_9").await.unwrap().data;
    assert_eq!(k.custody_mode.as_deref(), Some("BYOK"));
    assert_eq!(k.rotated_at.as_deref(), Some("2026-10-20T08:00:00.000Z"));
    let b = k.byok_ref.unwrap();
    assert_eq!(b.provider.as_deref(), Some("aws-kms"));
    assert_eq!(b.region.as_deref(), Some("eu-central-1"));
    assert_eq!(
        b.external_key_fingerprint.as_deref(),
        Some("AbCdEfGh01234567")
    );
    assert!(k.hyok_ref.is_none());
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/vault/keys/vkey_9")
    );

    let (_s, c) = setup(json_reply(
        200,
        json!({"data": {"id": "vkey_8", "version": 1, "status": "active", "custodyMode": "HYOK",
                         "createdAt": null, "rotatedAt": null, "byokRef": null,
                         "hyokRef": {"clusterId": "hsm_1", "clusterName": "berlin-hsm", "externalKeyFingerprint": "0123456789abcdef"}}, "meta": {}}),
    ))
    .await;
    let k = c.keys().get("vkey_8").await.unwrap().data;
    let h = k.hyok_ref.unwrap();
    assert_eq!(h.cluster_id.as_deref(), Some("hsm_1"));
    assert_eq!(h.cluster_name.as_deref(), Some("berlin-hsm"));
    assert_eq!(
        h.external_key_fingerprint.as_deref(),
        Some("0123456789abcdef")
    );
    assert!(k.created_at.is_none());
}

#[tokio::test]
async fn get_unknown_key_is_404() {
    let (_s, c) = setup(json_reply(
        404,
        json!({"code": "K9O_NOT_FOUND", "message": "no key"}),
    ))
    .await;
    let e = c.keys().get("vkey_x").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_NOT_FOUND", Some(404)));
}

#[tokio::test]
async fn a_non_person_credential_is_refused() {
    let (_s, c) = setup(json_reply(
        403,
        json!({"code": "K9O_TRUST_ZONE_VIOLATION", "message": "not for service tokens"}),
    ))
    .await;
    assert_eq!(
        c.keys().list(None).await.unwrap_err().code,
        "K9O_TRUST_ZONE_VIOLATION"
    );
    assert_eq!(
        c.keys()
            .rotate("vkey_1", RotateOptions::default())
            .await
            .unwrap_err()
            .status,
        Some(403)
    );
}

#[tokio::test]
async fn rotate_posts_without_a_body_and_decodes_the_answer() {
    let (server, c) = setup(json_reply(
        200,
        json!({"data": {"keyId": "vkey_1", "previousVersion": 1, "newVersion": 2, "status": "rotated"}, "meta": {}}),
    ))
    .await;
    let r = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap()
        .data;
    assert_eq!(r.key_id, "vkey_1");
    assert_eq!((r.previous_version, r.new_version), (Some(1), Some(2)));
    assert_eq!(r.status.as_deref(), Some("rotated"));
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("POST", "/@portal/vault/keys/vkey_1/rotate")
    );
    assert!(req.body.is_empty());
}

#[tokio::test]
async fn rotate_step_up_challenge_is_a_typed_error_with_the_challenge() {
    let (server, c) = setup(json_reply(
        401,
        json!({"code": "K9O_STEP_UP_REQUIRED", "message": "prove it with a passkey",
               "context": {"challenge": "ch_abc", "factor": "passkey"}}),
    ))
    .await;
    let e = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("K9O_STEP_UP_REQUIRED", Some(401))
    );
    assert_eq!(e.context["challenge"], "ch_abc");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotate_409_in_flight_and_not_active() {
    for reason in ["rotation_in_flight", "key_not_active"] {
        let (_s, c) = setup(json_reply(
            409,
            json!({"code": "K9O_STATE_CONFLICT", "message": "no", "context": {"reason": reason}}),
        ))
        .await;
        let e = c
            .keys()
            .rotate("vkey_1", RotateOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.status),
            ("K9O_STATE_CONFLICT", Some(409))
        );
        assert_eq!(e.context["reason"], reason);
    }
}

#[tokio::test]
async fn rotate_429_surfaces_the_retry_after_header() {
    let (server, c) = setup(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7200")
            .set_body_json(
                json!({"code": "K9O_RATE_LIMITED", "message": "3 rotations in 24 hours",
                                    "context": {"reason": "rotation_rate_cap"}}),
            ),
    )
    .await;
    let e = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_RATE_LIMITED", Some(429)));
    assert_eq!(e.context["reason"], "rotation_rate_cap");
    assert_eq!(e.retry_after_sec, Some(7200.0));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a POST is not retried"
    );
}

#[tokio::test]
async fn rotate_429_recommendation_retry_after_wins_over_the_header() {
    let (_s, c) = setup(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7200")
            .set_body_json(json!({"code": "K9O_RATE_LIMITED", "message": "x",
                                    "recommendation": {"retryAfterSec": 30, "note": "later"}})),
    )
    .await;
    let e = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.retry_after_sec, Some(30.0));
}

#[tokio::test]
async fn a_bad_retry_after_header_is_ignored() {
    let (_s, c) = setup(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")
            .set_body_json(json!({"code": "K9O_RATE_LIMITED", "message": "x"})),
    )
    .await;
    assert_eq!(
        c.keys()
            .rotate("vkey_1", RotateOptions::default())
            .await
            .unwrap_err()
            .retry_after_sec,
        None
    );
}

#[tokio::test]
async fn upstream_errors_are_typed() {
    let (_s, c) = setup(json_reply(
        502,
        json!({"code": "K9O_UPSTREAM_UNAVAILABLE", "message": "key service down"}),
    ))
    .await;
    assert_eq!(c.keys().get("vkey_1").await.unwrap_err().status, Some(502));
    let (_s, c) = setup(json_reply(
        500,
        json!({"code": "K9O_INTERNAL_ERROR", "message": "audit", "context": {"reason": "vault_audit_write_failed"}}),
    ))
    .await;
    let e = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.context["reason"], "vault_audit_write_failed");
}

#[tokio::test]
async fn key_id_is_checked_and_encoded() {
    let (server, c) = setup(json_reply(
        200,
        json!({"data": key("vkey_1", 1, "active"), "meta": {}}),
    ))
    .await;
    assert!(c.keys().get("").await.is_err());
    assert!(c
        .keys()
        .rotate("..", RotateOptions::default())
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    c.keys().get("a/b").await.unwrap();
    assert_eq!(
        server.received_requests().await.unwrap()[0].url.path(),
        "/@portal/vault/keys/a%2Fb"
    );
}

// ---- step-up -----------------------------------------------------------------------------------

fn challenge_body() -> Value {
    json!({"code": "K9O_STEP_UP_REQUIRED", "message": "step up",
           "context": {"stepUp": {"scopeSetRoot": "root_1", "nonce": "n_1", "minTier": "T3_KEY_POSSESSION",
                                  "methods": ["webauthn", "dpop"], "maxAgeSec": 120, "singleUse": true,
                                  "rpId": "k9o.example"}}})
}

fn decode_header(v: &str) -> Value {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(v)
        .expect("base64url without padding");
    serde_json::from_slice(&bytes).unwrap()
}

/// First request answers the challenge, the retried one succeeds.
async fn stepped(second: ResponseTemplate) -> (MockServer, Client) {
    use wiremock::matchers::header_exists;
    let server = MockServer::start().await;
    Mock::given(header_exists("x-k9o-step-up-proof"))
        .respond_with(second)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(json_reply(401, challenge_body()))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        retry_config: Some(RetryConfig {
            max_retries: 0,
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

#[tokio::test]
async fn rotate_answers_the_challenge_with_the_hook_and_retries_once() {
    let (server, c) = stepped(json_reply(
        200,
        json!({"data": {"keyId": "vkey_1", "previousVersion": 1, "newVersion": 2, "status": "rotated"}, "meta": {}}),
    ))
    .await;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<k9o::StepUpRequest>>> = Default::default();
    let log = seen.clone();
    let opts = RotateOptions::new().step_up(move |r| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(r);
            Ok(StepUpProof {
                method: "dpop".into(),
                proof: "proof.value".into(),
            })
        }
    });
    let r = c.keys().rotate("vkey_1", opts).await.unwrap().data;
    assert_eq!(r.new_version, Some(2));
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].challenge.scope_set_root, "root_1");
    assert_eq!(got[0].challenge.nonce, "n_1");
    assert_eq!(got[0].challenge.methods, ["webauthn", "dpop"]);
    assert_eq!(got[0].challenge.max_age_sec, Some(120));
    assert_eq!(got[0].challenge.rp_id.as_deref(), Some("k9o.example"));
    assert_eq!(got[0].http_method, "POST");
    assert_eq!(
        got[0].url,
        format!("{}/@portal/vault/keys/vkey_1/rotate", server.uri())
    );
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].headers.get("x-k9o-step-up-proof").is_none());
    let h = reqs[1].headers.get("x-k9o-step-up-proof").unwrap();
    assert_eq!(
        decode_header(h.to_str().unwrap()),
        json!({"scopeSetRoot": "root_1", "nonce": "n_1", "method": "dpop", "proof": "proof.value"})
    );
    assert_eq!(reqs[1].url.path(), "/@portal/vault/keys/vkey_1/rotate");
}

#[tokio::test]
async fn rotate_without_a_hook_throws_the_typed_error_with_the_challenge() {
    let (server, c) = stepped(json_reply(200, json!({"data": {}, "meta": {}}))).await;
    let e = c
        .keys()
        .rotate("vkey_1", RotateOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_STEP_UP_REQUIRED");
    let ch = StepUpChallenge::from_error(&e).unwrap();
    assert_eq!(
        (ch.scope_set_root.as_str(), ch.nonce.as_str()),
        ("root_1", "n_1")
    );
    assert_eq!(ch.min_tier.as_deref(), Some("T3_KEY_POSSESSION"));
    assert_eq!(e.context["stepUp"]["nonce"], "n_1");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotate_hook_failure_is_returned_and_nothing_is_retried() {
    let (server, c) = stepped(json_reply(200, json!({"data": {}, "meta": {}}))).await;
    let opts = RotateOptions::new().step_up(|_| async {
        Err::<StepUpProof, _>(KError::new("APP_PASSKEY_CANCELLED", "user cancelled"))
    });
    let e = c.keys().rotate("vkey_1", opts).await.unwrap_err();
    assert_eq!(e.code, "APP_PASSKEY_CANCELLED");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotate_a_method_that_was_not_offered_is_refused_before_sending() {
    let (server, c) = stepped(json_reply(200, json!({"data": {}, "meta": {}}))).await;
    let opts = RotateOptions::new().step_up(|_| async {
        Ok(StepUpProof {
            method: "mtls".into(),
            proof: "p".into(),
        })
    });
    let e = c.keys().rotate("vkey_1", opts).await.unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotate_a_second_401_is_not_retried() {
    for code in ["K9O_STEP_UP_PROOF_MALFORMED", "K9O_STEP_UP_REQUIRED"] {
        let (server, c) = stepped(json_reply(
            401,
            json!({"code": code, "message": "no",
                   "context": {"stepUp": {"scopeSetRoot": "r2", "nonce": "n2", "methods": ["dpop"]}}}),
        ))
        .await;
        let calls: std::sync::Arc<std::sync::Mutex<u32>> = Default::default();
        let n = calls.clone();
        let opts = RotateOptions::new().step_up(move |_| {
            let n = n.clone();
            async move {
                *n.lock().unwrap() += 1;
                Ok(StepUpProof {
                    method: "dpop".into(),
                    proof: "p".into(),
                })
            }
        });
        let e = c.keys().rotate("vkey_1", opts).await.unwrap_err();
        assert_eq!((e.code.as_str(), e.status), (code, Some(401)));
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn rotate_hook_is_not_called_for_other_errors() {
    let (server, c) = setup(json_reply(
        409,
        json!({"code": "K9O_STATE_CONFLICT", "message": "x", "context": {"reason": "key_not_active"}}),
    ))
    .await;
    let opts = RotateOptions::new().step_up(|_| async { panic!("hook must not run") });
    let e = c.keys().rotate("vkey_1", opts).await.unwrap_err();
    assert_eq!(e.code, "K9O_STATE_CONFLICT");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn list_tolerates_a_next_cursor_it_does_not_expose() {
    let (_s, c) = setup(json_reply(
        200,
        json!({"data": {"items": [key("vkey_1", 1, "active")], "nextCursor": "x"}, "meta": {}}),
    ))
    .await;
    assert_eq!(c.keys().list(None).await.unwrap().data.items.len(), 1);
}
