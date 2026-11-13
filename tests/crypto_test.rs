//! Server-side encryption: `crypto().encrypt / decrypt / rewrap` over a stubbed transport.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use k9o::{
    key_ref_matches, Client, ClientCore, ClientOpts, CoreOptions, CryptoBatch, CryptoItemResult,
    KError, PlainItem, RetryConfig, SealedItem,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn mode_reply(mode: &str, epoch: &str) -> ResponseTemplate {
    reply(
        200,
        json!({"data": {"custodyMode": mode, "epoch": epoch}, "meta": {}}),
    )
}

fn route_reply(mode: &str, epoch: &str, route: Value) -> ResponseTemplate {
    let mut d = json!({"custodyMode": mode, "epoch": epoch});
    d["route"] = route;
    reply(200, json!({"data": d, "meta": {}}))
}

/// A server whose mode check says platform-managed and whose other routes answer `t`.
async fn setup(t: ResponseTemplate) -> (MockServer, Client) {
    setup_mode(mode_reply("PLATFORM_MANAGED", "e1"), t).await
}

async fn setup_mode(mode: ResponseTemplate, t: ResponseTemplate) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(mode)
        .mount(&server)
        .await;
    Mock::given(any()).respond_with(t).mount(&server).await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        // The default retry policy: a POST must still not be retried.
        retry_config: Some(RetryConfig::default()),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

fn reply(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

async fn posts(server: &MockServer) -> Vec<Request> {
    let r = server.received_requests().await.unwrap();
    r.into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .collect()
}

async fn mode_gets(server: &MockServer) -> usize {
    let r = server.received_requests().await.unwrap();
    r.iter()
        .filter(|r| r.url.path().ends_with("/crypto/mode"))
        .count()
}

async fn only_body(server: &MockServer) -> (String, Value) {
    let r = posts(server).await;
    assert_eq!(r.len(), 1);
    (
        format!("{} {}", r[0].method, r[0].url.path()),
        serde_json::from_slice(&r[0].body).unwrap(),
    )
}

#[tokio::test]
async fn encrypt_sends_base64_plaintext_and_returns_ciphertexts_in_order() {
    let (server, c) = setup(reply(
        200,
        json!({"data": {"items": [{"ciphertext": "Y3Q6MQ=="}, {"ciphertext": "Y3Q6Mg==", "kv": 3}]}, "meta": {}}),
    ))
    .await;
    let r = c
        .crypto()
        .encrypt(
            "payments-1",
            &[
                PlainItem::text("alpha").context("order:7"),
                PlainItem::new(vec![0u8, 255, 1]),
            ],
        )
        .await
        .unwrap()
        .data;
    assert_eq!(r.items[0].ciphertext.as_deref(), Some("Y3Q6MQ=="));
    assert_eq!(r.items[1].ciphertext.as_deref(), Some("Y3Q6Mg=="));
    assert_eq!(r.items[1].extra["kv"], 3);
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/encrypt");
    assert_eq!(
        body,
        json!({"crypt": "payments-1", "custodyEpoch": "e1", "items": [
            {"plaintext": STANDARD.encode("alpha"), "context": "order:7"},
            {"plaintext": STANDARD.encode([0u8, 255, 1])}
        ]})
    );
}

#[tokio::test]
async fn decrypt_decodes_plaintext_and_keeps_per_item_failures_in_order() {
    let (server, c) = setup(reply(
        200,
        json!({"data": {"items": [
            {"plaintext": STANDARD.encode("hello")},
            {"code": "K9O_ENVELOPE_INTEGRITY_FAILURE", "message": "cannot open"},
            {"plaintext": STANDARD.encode([0xffu8, 0xfe])}
        ]}, "meta": {}}),
    ))
    .await;
    let r = c
        .crypto()
        .decrypt(
            "payments-1",
            &[
                SealedItem::new("ct1").context("order:7"),
                SealedItem::new("ct2"),
                SealedItem::new("ct3"),
            ],
        )
        .await
        .unwrap()
        .data;
    assert_eq!(r.items[0].plaintext_text().as_deref(), Some("hello"));
    assert!(r.items[1].is_error());
    assert_eq!(
        r.items[1].error().unwrap().code,
        "K9O_ENVELOPE_INTEGRITY_FAILURE"
    );
    assert_eq!(r.items[2].plaintext_bytes(), Some(vec![0xff, 0xfe]));
    assert_eq!(r.items[2].plaintext_text(), None);
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/decrypt");
    assert_eq!(
        body,
        json!({"crypt": "payments-1", "items": [
            {"ciphertext": "ct1", "context": "order:7"}, {"ciphertext": "ct2"}, {"ciphertext": "ct3"}
        ]})
    );
}

#[tokio::test]
async fn rewrap_posts_ciphertexts_and_returns_new_ones() {
    let (server, c) = setup(reply(
        200,
        json!({"data": {"items": [{"ciphertext": "new"}]}, "meta": {}}),
    ))
    .await;
    let r = c
        .crypto()
        .rewrap("k", &[SealedItem::new("old")])
        .await
        .unwrap()
        .data;
    assert_eq!(r.items[0].ciphertext.as_deref(), Some("new"));
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/rewrap");
    assert_eq!(
        body,
        json!({"crypt": "k", "items": [{"ciphertext": "old"}]})
    );
}

#[tokio::test]
async fn validation_happens_before_anything_is_sent() {
    let (server, c) = setup(reply(200, json!({"data": {"items": []}, "meta": {}}))).await;
    let one = || vec![PlainItem::text("x")];
    let code = |r: Result<_, k9o::KError>| -> String {
        r.map(|_: k9o::Response<k9o::CryptoBatch>| ())
            .unwrap_err()
            .code
    };
    for bad in [
        "",
        "Upper",
        "has_underscore",
        "has space",
        "actions",
        &"a".repeat(65),
    ] {
        assert_eq!(
            code(c.crypto().encrypt(bad, &one()).await),
            "K9O_INVALID_FIELD_VALUE"
        );
        assert_eq!(
            code(c.crypto().decrypt(bad, &[SealedItem::new("c")]).await),
            "K9O_INVALID_FIELD_VALUE"
        );
    }
    assert_eq!(
        code(c.crypto().encrypt("k", &[]).await),
        "K9O_INVALID_FIELD_VALUE"
    );
    let many: Vec<PlainItem> = (0..101).map(|_| PlainItem::text("x")).collect();
    assert_eq!(
        code(c.crypto().encrypt("k", &many).await),
        "K9O_INVALID_FIELD_VALUE"
    );
    let many: Vec<SealedItem> = (0..101).map(|_| SealedItem::new("c")).collect();
    assert_eq!(
        code(c.crypto().rewrap("k", &many).await),
        "K9O_INVALID_FIELD_VALUE"
    );
    assert_eq!(
        code(
            c.crypto()
                .encrypt("k", &[PlainItem::new(vec![0u8; 65537])])
                .await
        ),
        "K9O_PAYLOAD_TOO_LARGE"
    );
    // 17 items of 64 KiB is 1088 KiB: over the 1 MiB call limit, each item within its own.
    let heavy: Vec<PlainItem> = (0..17).map(|_| PlainItem::new(vec![0u8; 65536])).collect();
    assert_eq!(
        code(c.crypto().encrypt("k", &heavy).await),
        "K9O_PAYLOAD_TOO_LARGE"
    );
    assert_eq!(
        code(
            c.crypto()
                .encrypt("k", &[PlainItem::text("x").context("c".repeat(257))])
                .await
        ),
        "K9O_INVALID_FIELD_VALUE"
    );
    assert_eq!(
        code(
            c.crypto()
                .decrypt("k", &[SealedItem::new("c").context("c".repeat(257))])
                .await
        ),
        "K9O_INVALID_FIELD_VALUE"
    );
    assert_eq!(
        code(c.crypto().decrypt("k", &[SealedItem::new("")]).await),
        "K9O_INVALID_FIELD_VALUE"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    // The limits themselves are accepted.
    let ok: Vec<PlainItem> = (0..16).map(|_| PlainItem::new(vec![0u8; 65536])).collect();
    c.crypto().encrypt("k", &ok).await.unwrap();
    c.crypto()
        .encrypt("a-1", &[PlainItem::text("x").context("c".repeat(256))])
        .await
        .unwrap();
    assert_eq!(posts(&server).await.len(), 2);
}

#[tokio::test]
async fn each_error_is_typed_and_a_5xx_is_not_retried() {
    let cases: [(u16, &str); 6] = [
        (400, "K9O_ENVELOPE_INTEGRITY_FAILURE"),
        (409, "K9O_SEALED_UNDER_RETIRED_KEY"),
        (413, "K9O_PAYLOAD_TOO_LARGE"),
        (503, "K9O_KEY_SERVICE_UNAVAILABLE"),
        (503, "K9O_BYOK_KEY_UNREACHABLE"),
        (503, "K9O_HYOK_CLUSTER_UNREACHABLE"),
    ];
    for (status, code) in cases {
        for verb in 0..3 {
            let (server, c) = setup(reply(
                status,
                json!({"code": code, "message": "m", "context": {"resealableUntil": "2026-11-20T00:00:00Z"}}),
            ))
            .await;
            let items = [SealedItem::new("c")];
            let e = match verb {
                0 => c.crypto().decrypt("k", &items).await.unwrap_err(),
                1 => c.crypto().rewrap("k", &items).await.unwrap_err(),
                _ => c
                    .crypto()
                    .encrypt("k", &[PlainItem::text("x")])
                    .await
                    .unwrap_err(),
            };
            assert_eq!((e.code.as_str(), e.status), (code, Some(status)));
            if status == 409 {
                assert_eq!(e.context["resealableUntil"], "2026-11-20T00:00:00Z");
            }
            assert_eq!(posts(&server).await.len(), 1, "{code}");
        }
    }
}

#[tokio::test]
async fn rate_limit_carries_retry_after_and_is_not_retried() {
    let (server, c) = setup(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7")
            .set_body_json(json!({"code": "K9O_RATE_LIMITED", "message": "slow down"})),
    )
    .await;
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_RATE_LIMITED");
    assert_eq!(e.retry_after_sec, Some(7.0));
    assert_eq!(posts(&server).await.len(), 1);
}

#[tokio::test]
async fn a_404_means_the_gateway_does_not_serve_the_route_yet() {
    let (_s, c) = setup(reply(
        404,
        json!({"code": "K9O_NOT_FOUND", "message": "no such route"}),
    ))
    .await;
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_NOT_FOUND", Some(404)));
}

#[tokio::test]
async fn no_project_is_a_typed_error() {
    let c = Client::new(ClientOpts {
        base_url: Some("http://127.0.0.1:9".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}

// ---- the custody router ----

#[derive(Default)]
struct FakeCore {
    calls: Mutex<Vec<String>>,
    seen: Mutex<Vec<Option<Vec<String>>>>,
    /// When true the core does not declare that it enforces the pin.
    undeclared: bool,
}

impl FakeCore {
    fn note(&self, op: &str, o: &CoreOptions<'_>) {
        self.calls.lock().unwrap().push(op.into());
        self.seen
            .lock()
            .unwrap()
            .push(o.expected_key_refs.map(|r| r.to_vec()));
    }
}

fn one_result() -> CryptoBatch {
    CryptoBatch {
        items: vec![
            serde_json::from_value::<CryptoItemResult>(json!({"ciphertext": "from-core"})).unwrap(),
        ],
    }
}

#[async_trait::async_trait]
impl ClientCore for FakeCore {
    fn enforces_expected_key_refs(&self) -> bool {
        !self.undeclared
    }
    async fn encrypt(
        &self,
        _k: &str,
        _i: &[PlainItem],
        o: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        self.note("encrypt", o);
        Ok(one_result())
    }
    async fn decrypt(
        &self,
        _k: &str,
        _i: &[SealedItem],
        o: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        self.note("decrypt", o);
        Ok(one_result())
    }
    async fn rewrap(
        &self,
        _k: &str,
        _i: &[SealedItem],
        o: &CoreOptions<'_>,
    ) -> Result<CryptoBatch, KError> {
        self.note("rewrap", o);
        Ok(one_result())
    }
}

fn flow_required(route: &str, epoch: &str) -> ResponseTemplate {
    reply(
        409,
        json!({"code": "K9O_ZK_STORE_FLOW_REQUIRED", "message": "m",
               "context": {"custodyMode": "BYOK", "route": route, "epoch": epoch}}),
    )
}

fn ok_batch() -> ResponseTemplate {
    reply(
        200,
        json!({"data": {"items": [{"ciphertext": "srv"}]}, "meta": {}}),
    )
}

#[tokio::test]
async fn platform_managed_encrypt_is_one_mode_get_then_one_post_with_the_epoch() {
    let (server, c) = setup(ok_batch()).await;
    c.crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap();
    assert_eq!(mode_gets(&server).await, 1);
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/encrypt");
    assert_eq!(body["custodyEpoch"], "e1");
    assert_eq!(body["crypt"], "k");
    assert_eq!(body.as_object().unwrap().len(), 3);
}

#[tokio::test]
async fn encrypt_mode_check_names_the_crypt_and_mode_does_not() {
    let (server, c) = setup(ok_batch()).await;
    c.crypto()
        .encrypt("customer-notes", &[PlainItem::text("x")])
        .await
        .unwrap();
    c.crypto().mode().await.unwrap();
    let r = server.received_requests().await.unwrap();
    let queries: Vec<Option<String>> = r
        .iter()
        .filter(|r| r.url.path().ends_with("/crypto/mode"))
        .map(|r| r.url.query().map(str::to_string))
        .collect();
    assert_eq!(queries, vec![Some("crypt=customer-notes".to_string()), None]);
}

#[tokio::test]
async fn mode_for_sends_the_crypt_and_refuses_a_bad_name_before_any_request() {
    let (server, c) = setup(ok_batch()).await;
    c.crypto().mode_for("customer-notes").await.unwrap();
    c.crypto().mode().await.unwrap();
    for bad in ["Bad_Name", "", "actions"] {
        let e = c.crypto().mode_for(bad).await.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE", "{bad}");
    }
    let r = server.received_requests().await.unwrap();
    let queries: Vec<Option<String>> = r
        .iter()
        .filter(|r| r.url.path().ends_with("/crypto/mode"))
        .map(|r| r.url.query().map(str::to_string))
        .collect();
    assert_eq!(queries, vec![Some("crypt=customer-notes".to_string()), None]);
}

#[tokio::test]
async fn confidential_workload_and_skr_use_the_server_route() {
    for m in ["CONFIDENTIAL_WORKLOAD", "SKR"] {
        let (server, c) = setup_mode(mode_reply(m, "e9"), ok_batch()).await;
        c.crypto()
            .encrypt("k", &[PlainItem::text("x")])
            .await
            .unwrap();
        assert_eq!(posts(&server).await.len(), 1, "{m}");
    }
}

#[tokio::test]
async fn two_encrypts_make_two_mode_checks() {
    let (server, c) = setup(ok_batch()).await;
    c.crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap();
    c.crypto()
        .encrypt("k", &[PlainItem::text("y"), PlainItem::text("z")])
        .await
        .unwrap();
    assert_eq!(mode_gets(&server).await, 2);
    assert_eq!(posts(&server).await.len(), 2);
}

#[tokio::test]
async fn mode_returns_the_typed_state() {
    let (_s, c) = setup(ok_batch()).await;
    let m = c.crypto().mode().await.unwrap().data;
    assert_eq!(
        (m.custody_mode.as_str(), m.epoch.as_str()),
        ("PLATFORM_MANAGED", "e1")
    );
}

#[tokio::test]
async fn byok_and_hyok_without_a_core_raise_and_send_no_plaintext() {
    for m in ["BYOK", "HYOK"] {
        let (server, c) = setup_mode(mode_reply(m, "e2"), ok_batch()).await;
        let e = c
            .crypto()
            .encrypt("k", &[PlainItem::text("secret")])
            .await
            .unwrap_err();
        assert_eq!(e.code, "K9O_ZK_CLIENT_CORE_UNAVAILABLE");
        assert_eq!(e.status, Some(501));
        assert_eq!(e.context["custodyMode"], m);
        assert_eq!(e.context["requirement"], "kibo-client-core");
        assert!(e.message.contains("Kibo client core"));
        assert!(posts(&server).await.is_empty(), "{m}");
    }
}

#[tokio::test]
async fn byok_with_a_core_calls_the_core_and_no_server_encrypt() {
    let (server, c) = setup_mode(mode_reply("BYOK", "e2"), ok_batch()).await;
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    let r = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap()
        .data;
    assert_eq!(r.items[0].ciphertext.as_deref(), Some("from-core"));
    assert_eq!(*core.calls.lock().unwrap(), vec!["encrypt".to_string()]);
    assert!(posts(&server).await.is_empty());
}

#[tokio::test]
async fn an_epoch_race_re_checks_once_and_reroutes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(mode_reply("PLATFORM_MANAGED", "e1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(mode_reply("BYOK", "e2"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(flow_required("client", "e2"))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    let r = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap()
        .data;
    assert_eq!(r.items[0].ciphertext.as_deref(), Some("from-core"));
    assert_eq!(mode_gets(&server).await, 2);
    assert_eq!(posts(&server).await.len(), 1);
}

#[tokio::test]
async fn a_second_409_is_raised_after_one_re_route() {
    let (server, c) = setup(flow_required("client", "e3")).await;
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("K9O_ZK_STORE_FLOW_REQUIRED", Some(409))
    );
    assert_eq!(mode_gets(&server).await, 2);
    assert_eq!(posts(&server).await.len(), 2);
}

#[tokio::test]
async fn unknown_or_failing_mode_fails_closed_with_nothing_sent() {
    let bad = [
        mode_reply("QUANTUM", "e"),
        reply(200, json!({"data": {}, "meta": {}})),
        reply(404, json!({"code": "K9O_NOT_FOUND", "message": "no"})),
        reply(500, json!({"code": "K9O_INTERNAL_ERROR", "message": "no"})),
    ];
    for m in bad {
        let (server, c) = setup_mode(m, ok_batch()).await;
        c.crypto().use_client_core(Arc::new(FakeCore::default()));
        assert!(c
            .crypto()
            .encrypt("k", &[PlainItem::text("x")])
            .await
            .is_err());
        assert!(posts(&server).await.is_empty());
    }
    let (_s, c) = setup_mode(mode_reply("QUANTUM", "e"), ok_batch()).await;
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_CRYPTO_CUSTODY_UNKNOWN");
}

#[tokio::test]
async fn decrypt_and_rewrap_reroute_on_a_client_409_only() {
    for verb in ["decrypt", "rewrap"] {
        // 200: no core, no mode call.
        let (server, c) = setup(ok_batch()).await;
        let core = Arc::new(FakeCore::default());
        c.crypto().use_client_core(core.clone());
        let items = [SealedItem::new("ct")];
        let r = if verb == "decrypt" {
            c.crypto().decrypt("k", &items).await
        } else {
            c.crypto().rewrap("k", &items).await
        };
        assert_eq!(r.unwrap().data.items[0].ciphertext.as_deref(), Some("srv"));
        assert!(core.calls.lock().unwrap().is_empty());
        assert_eq!(mode_gets(&server).await, 0);

        // 409 route client: the core opens it, after one POST.
        let (server, c) = setup(flow_required("client", "e2")).await;
        let core = Arc::new(FakeCore::default());
        c.crypto().use_client_core(core.clone());
        let r = if verb == "decrypt" {
            c.crypto().decrypt("k", &items).await
        } else {
            c.crypto().rewrap("k", &items).await
        };
        assert_eq!(
            r.unwrap().data.items[0].ciphertext.as_deref(),
            Some("from-core")
        );
        assert_eq!(*core.calls.lock().unwrap(), vec![verb.to_string()]);
        assert_eq!(posts(&server).await.len(), 1);
        assert_eq!(mode_gets(&server).await, 0);

        // 409 route client with no core: typed 501.
        let (_s, c) = setup(flow_required("client", "e2")).await;
        let e = if verb == "decrypt" {
            c.crypto().decrypt("k", &items).await
        } else {
            c.crypto().rewrap("k", &items).await
        }
        .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.status),
            ("K9O_ZK_CLIENT_CORE_UNAVAILABLE", Some(501))
        );

        // Other 409 and 400 are raised unchanged, core untouched.
        for t in [
            flow_required("server", "e2"),
            reply(
                409,
                json!({"code": "K9O_SEALED_UNDER_RETIRED_KEY", "message": "m"}),
            ),
            reply(
                400,
                json!({"code": "K9O_ENVELOPE_INTEGRITY_FAILURE", "message": "m"}),
            ),
        ] {
            let (_s, c) = setup(t).await;
            let core = Arc::new(FakeCore::default());
            c.crypto().use_client_core(core.clone());
            let e = if verb == "decrypt" {
                c.crypto().decrypt("k", &items).await
            } else {
                c.crypto().rewrap("k", &items).await
            }
            .unwrap_err();
            assert_ne!(e.code, "K9O_ZK_CLIENT_CORE_UNAVAILABLE");
            assert!(core.calls.lock().unwrap().is_empty());
        }
    }
}

// ---- expected_key_refs ----

const KEY: &str = "arn:aws:kms:eu-west-1:111122223333:key/1234abcd";

async fn byok_client(refs: Option<Vec<String>>) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(mode_reply("BYOK", "e2"))
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ok_batch())
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        expected_key_refs: refs,
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

#[tokio::test]
async fn expected_key_refs_reach_the_core_on_encrypt() {
    let (_s, c) = byok_client(Some(vec![KEY.to_string()])).await;
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    c.crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap();
    assert_eq!(
        *core.seen.lock().unwrap(),
        vec![Some(vec![KEY.to_string()])]
    );
}

#[tokio::test]
async fn expected_key_refs_reach_the_core_on_decrypt_and_rewrap() {
    let items = [SealedItem::new("ct")];
    let flow = flow_required("client", "e2");
    let server = MockServer::start().await;
    Mock::given(any()).respond_with(flow).mount(&server).await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        expected_key_refs: Some(vec![KEY.to_string()]),
        ..Default::default()
    })
    .unwrap();
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    c.crypto().decrypt("k", &items).await.unwrap();
    c.crypto().rewrap("k", &items).await.unwrap();
    let want = Some(vec![KEY.to_string()]);
    assert_eq!(*core.seen.lock().unwrap(), vec![want.clone(), want]);
}

#[tokio::test]
async fn expected_key_refs_are_absent_when_unset() {
    let (_s, c) = byok_client(None).await;
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    c.crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap();
    assert_eq!(*core.seen.lock().unwrap(), vec![None]);
}

#[test]
fn invalid_expected_key_refs_are_rejected_when_the_client_is_built() {
    let cases: Vec<Vec<String>> = vec![
        vec![],
        vec!["".into()],
        vec!["   ".into()],
        vec!["arn:aws:kms:eu-west-1:1:key/a b".into()],
        vec!["arn:aws:kms:*:1:key/a".into()],
        vec![KEY.into(), "*".into()],
    ];
    for refs in cases {
        let e = Client::new(ClientOpts {
            expected_key_refs: Some(refs.clone()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE", "{refs:?}");
        assert!(e.message.starts_with("expectedKeyRefs: "), "{}", e.message);
        assert_eq!(e.context["field"], "expectedKeyRefs");
    }
}

#[tokio::test]
async fn an_undeclared_core_is_refused_on_encrypt_when_a_list_is_set() {
    let (_s, c) = byok_client(Some(vec![KEY.to_string()])).await;
    let core = Arc::new(FakeCore {
        undeclared: true,
        ..Default::default()
    });
    c.crypto().use_client_core(core.clone());
    let e = c
        .crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_pin_unenforced(&e);
    assert_eq!(e.context["custodyMode"], "BYOK");
    assert!(core.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_undeclared_core_is_refused_on_decrypt_and_rewrap_when_a_list_is_set() {
    let items = [SealedItem::new("ct")];
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(flow_required("client", "e2"))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        expected_key_refs: Some(vec![KEY.to_string()]),
        ..Default::default()
    })
    .unwrap();
    let core = Arc::new(FakeCore {
        undeclared: true,
        ..Default::default()
    });
    c.crypto().use_client_core(core.clone());
    assert_pin_unenforced(&c.crypto().decrypt("k", &items).await.unwrap_err());
    assert_pin_unenforced(&c.crypto().rewrap("k", &items).await.unwrap_err());
    assert!(core.calls.lock().unwrap().is_empty());
}

fn assert_pin_unenforced(e: &KError) {
    assert_eq!(e.code, "K9O_CRYPTO_KEY_PIN_UNENFORCED");
    assert_eq!(e.status, None);
    assert_eq!(
        e.context["requirement"],
        "client-core-enforces-expected-key-refs"
    );
    assert!(e.message.contains("does not declare that it enforces it"));
    assert!(e
        .recommendation
        .as_deref()
        .unwrap()
        .contains("enforces_expected_key_refs returns true"));
}

#[tokio::test]
async fn an_undeclared_core_works_when_no_list_is_set() {
    let (_s, c) = byok_client(None).await;
    let core = Arc::new(FakeCore {
        undeclared: true,
        ..Default::default()
    });
    c.crypto().use_client_core(core.clone());
    c.crypto()
        .encrypt("k", &[PlainItem::text("x")])
        .await
        .unwrap();
    assert_eq!(*core.calls.lock().unwrap(), vec!["encrypt".to_string()]);
}

#[test]
fn key_ref_matches_follows_the_provider_rules() {
    use k9o::KeyRefProvider::*;
    let exp = |s: &str| vec![s.to_string()];
    let gcp = "projects/p/locations/eu/keyRings/r/cryptoKeys/k";

    // Azure: first five parts, lower case; the version is ignored.
    let az = "https://Vault.Example.vault.azure.net/keys/Orders";
    assert!(key_ref_matches(
        Azure,
        "https://vault.example.vault.azure.net/keys/orders/0123456789abcdef",
        &exp(az)
    ));
    assert!(!key_ref_matches(
        Azure,
        "https://vault.example.vault.azure.net/keys/other",
        &exp(az)
    ));
    assert!(key_ref_matches(
        Azure,
        "HTTP://H/keys/K/v",
        &exp("http://h/keys/k")
    ));
    // Vault: <path>/<key>, ":<version>" cut.
    assert!(key_ref_matches(
        Vault,
        "transit/keys/orders:3",
        &exp("transit/keys/orders")
    ));
    assert!(!key_ref_matches(
        Vault,
        "transit/keys/orders2:3",
        &exp("transit/keys/orders")
    ));
    assert!(key_ref_matches(Vault, "orders:3", &exp("orders")));
    // GCP and AWS.
    assert!(key_ref_matches(
        Gcp,
        &format!("{gcp}/cryptoKeyVersions/9"),
        &exp(gcp)
    ));
    assert!(!key_ref_matches(
        Gcp,
        &format!("{gcp}x/cryptoKeyVersions/9"),
        &exp(gcp)
    ));
    assert!(key_ref_matches(Aws, KEY, &exp(KEY)));
    assert!(!key_ref_matches(
        Aws,
        KEY,
        &exp("arn:aws:kms:eu-west-1:111122223333:key/ffff")
    ));
    // The provider decides the rule: the same reference compares differently per provider.
    assert!(!key_ref_matches(
        Aws,
        "arn:a:b:c:d:key/x:1",
        &exp("arn:a:b:c:d:key/x")
    ));
    assert!(key_ref_matches(
        Vault,
        "arn:a:b:c:d:key/x:1",
        &exp("arn:a:b:c:d:key/x")
    ));
}

#[test]
fn a_url_shaped_vault_reference_compares_by_the_vault_rule_only() {
    use k9o::KeyRefProvider::*;
    let r = "https://vault.example.com:8200/transit/keys/orders:3";
    let exp = vec!["https://vault.example.com:8200/transit/keys/orders".to_string()];
    assert!(key_ref_matches(Vault, r, &exp));
    // A different key does not match by the Vault rule, but the Azure rule would keep only the
    // first five parts and wrongly accept it; AWS and GCP compare other things again.
    let other = vec!["https://vault.example.com:8200/transit/keys/billing".to_string()];
    assert!(!key_ref_matches(Vault, r, &other));
    assert!(key_ref_matches(Azure, r, &other));
    assert!(!key_ref_matches(Aws, r, &exp));
    assert!(!key_ref_matches(Gcp, r, &other));
}

fn fed_body() -> Value {
    json!({"data": {"token": "eyJ.secret.sig", "expiresAt": "2026-11-02T10:15:00Z",
        "issuer": "https://acme.api.k9o.io", "subject": "svc_1", "audience": "kms", "kid": "k1"}, "meta": {}})
}

#[tokio::test]
async fn federated_token_decodes_and_sends_exact_body() {
    let (server, c) = setup(reply(200, fed_body())).await;
    let r = c
        .crypto()
        .federated_token("customer-notes", "rewrap")
        .await
        .unwrap()
        .data;
    assert_eq!(
        (
            r.token.as_str(),
            r.expires_at.as_str(),
            r.issuer.as_str(),
            r.subject.as_str(),
            r.audience.as_str(),
            r.kid.as_str()
        ),
        (
            "eyJ.secret.sig",
            "2026-11-02T10:15:00Z",
            "https://acme.api.k9o.io",
            "svc_1",
            "kms",
            "k1"
        )
    );
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/federated-token");
    assert_eq!(body, json!({"crypt": "customer-notes", "op": "rewrap"}));
}

#[tokio::test]
async fn federated_token_provider_and_project_id_are_optional() {
    let (_s, c) = setup(reply(200, fed_body())).await;
    let r = c.crypto().federated_token("k", "encrypt").await.unwrap().data;
    assert_eq!((r.provider, r.project_id), (None, None));

    let mut body = fed_body();
    body["data"]["provider"] = json!("aws");
    body["data"]["projectId"] = json!("proj_1");
    let (_s, c) = setup(reply(200, body)).await;
    let r = c.crypto().federated_token("k", "encrypt").await.unwrap().data;
    assert_eq!(r.provider.as_deref(), Some("aws"));
    assert_eq!(r.project_id.as_deref(), Some("proj_1"));
}

#[tokio::test]
async fn federated_token_refuses_bad_crypt_and_op_before_sending() {
    let (server, c) = setup(reply(200, fed_body())).await;
    let long = "a".repeat(65);
    for (k, op) in [
        ("Bad Crypt", "encrypt"),
        ("", "encrypt"),
        (long.as_str(), "encrypt"),
        ("ok", "sign"),
        ("ok", ""),
        ("ok", "Encrypt"),
    ] {
        let e = c.crypto().federated_token(k, op).await.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE", "{k:?} {op:?}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn federated_token_409_and_503_pass_through_without_retry() {
    for (status, code) in [
        (409, "K9O_CUSTODY_SERVER_ROUTE"),
        (503, "K9O_KEY_SERVICE_UNAVAILABLE"),
    ] {
        let (server, c) = setup(reply(status, json!({"code": code, "message": "no"}))).await;
        let e = c
            .crypto()
            .federated_token("k", "encrypt")
            .await
            .unwrap_err();
        assert_eq!((e.code.as_str(), e.status), (code, Some(status)));
        assert_eq!(posts(&server).await.len(), 1);
    }
}

#[tokio::test]
async fn federated_token_missing_field_is_malformed() {
    for drop in ["token", "expiresAt", "issuer", "subject", "audience", "kid"] {
        let mut b = fed_body();
        b["data"].as_object_mut().unwrap().remove(drop);
        let (_s, c) = setup(reply(200, b)).await;
        let e = c
            .crypto()
            .federated_token("k", "decrypt")
            .await
            .unwrap_err();
        assert_eq!(e.code, "K9O_INTERNAL_ERROR", "{drop}");
        assert!(!e.message.contains("eyJ.secret.sig"));
    }
}

#[tokio::test]
async fn federated_token_debug_redacts_the_token() {
    let (_s, c) = setup(reply(200, fed_body())).await;
    let r = c.crypto().federated_token("k", "encrypt").await.unwrap();
    let shown = format!("{:?} {:?}", r.data, r);
    assert!(!shown.contains("eyJ.secret.sig"), "{shown}");
    assert!(shown.contains("<redacted>"));
}

#[tokio::test]
async fn actions_name_is_refused_with_the_exact_message_and_actions_2_is_sent() {
    let (server, c) = setup(reply(
        200,
        json!({"data": {"items": [{"ciphertext": "ct"}]}, "meta": {}}),
    ))
    .await;
    let e = c
        .crypto()
        .encrypt("actions", &[PlainItem::text("x")])
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert!(e
        .message
        .contains("crypt must be 1 to 64 characters of a-z, 0-9 and -, and not \"actions\""));
    assert!(c
        .crypto()
        .federated_token("actions", "encrypt")
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    c.crypto()
        .encrypt("actions-2", &[PlainItem::text("x")])
        .await
        .unwrap();
    let (_, body) = only_body(&server).await;
    assert_eq!(body["crypt"], "actions-2");
}

#[tokio::test]
async fn crypt_not_found_and_retired_pass_through_on_encrypt() {
    for (status, code) in [(404u16, "K9O_CRYPT_NOT_FOUND"), (409, "K9O_CRYPT_RETIRED")] {
        let (server, c) = setup(reply(status, json!({"code": code, "message": "m"}))).await;
        let e = c
            .crypto()
            .encrypt("orders", &[PlainItem::text("x")])
            .await
            .unwrap_err();
        assert_eq!((e.code.as_str(), e.status), (code, Some(status)));
        assert_eq!(posts(&server).await.len(), 1, "{code}");
    }
    assert_eq!(k9o::crypto::CODE_CRYPT_NOT_FOUND, "K9O_CRYPT_NOT_FOUND");
}

// ---- crypts().list / create / retire ----

fn crypt_json(name: &str, retired: bool) -> Value {
    json!({"name": name, "status": if retired {"retired"} else {"active"},
           "createdAt": "2026-11-03T00:00:00.000Z",
           "retiredAt": if retired { json!("2026-11-03T01:00:00.000Z") } else { Value::Null }})
}

async fn all_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| !r.url.path().ends_with("/crypto/mode"))
        .collect()
}

#[tokio::test]
async fn crypts_list_decodes_the_registry() {
    let (server, c) = setup(reply(
        200,
        json!({"data": {"crypts": [crypt_json("payments", false), crypt_json("old", true)]}, "meta": {}}),
    ))
    .await;
    let l = c.crypto().crypts().list().await.unwrap().data;
    assert_eq!(l.crypts.len(), 2);
    assert_eq!(l.crypts[0].name, "payments");
    assert_eq!(l.crypts[0].status, k9o::CryptStatus::Active);
    assert_eq!(l.crypts[0].retired_at, None);
    assert_eq!(l.crypts[1].status, k9o::CryptStatus::Retired);
    assert_eq!(
        l.crypts[1].retired_at.as_deref(),
        Some("2026-11-03T01:00:00.000Z")
    );
    let r = all_requests(&server).await;
    assert_eq!(r.len(), 1);
    assert_eq!(
        format!("{} {}", r[0].method, r[0].url.path()),
        "GET /@portal/crypto/crypts"
    );
}

#[tokio::test]
async fn crypts_list_empty() {
    let (_s, c) = setup(reply(200, json!({"data": {"crypts": []}, "meta": {}}))).await;
    assert!(c.crypto().crypts().list().await.unwrap().data.crypts.is_empty());
}

#[tokio::test]
async fn crypts_create_sends_only_name() {
    let (server, c) = setup(reply(201, json!({"data": crypt_json("payments", false), "meta": {}}))).await;
    let r = c.crypto().crypts().create("payments").await.unwrap().data;
    assert_eq!(r.name, "payments");
    let (line, body) = only_body(&server).await;
    assert_eq!(line, "POST /@portal/crypto/crypts");
    assert_eq!(body, json!({"name": "payments"}));
}

#[tokio::test]
async fn crypts_retire_sends_no_body_to_the_right_url() {
    let (server, c) = setup(reply(200, json!({"data": crypt_json("payments", true), "meta": {}}))).await;
    let r = c.crypto().crypts().retire("payments").await.unwrap().data;
    assert_eq!(r.status, k9o::CryptStatus::Retired);
    let p = posts(&server).await;
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].url.path(), "/@portal/crypto/crypts/payments/retire");
    assert!(p[0].body.is_empty());
}

#[tokio::test]
async fn crypts_bad_names_are_refused_before_sending() {
    let (server, c) = setup(reply(200, json!({"data": crypt_json("x", false), "meta": {}}))).await;
    let long = "a".repeat(65);
    for n in ["", "Bad", "has space", "actions", "a/b", "a_b", long.as_str()] {
        for e in [
            c.crypto().crypts().create(n).await.unwrap_err(),
            c.crypto().crypts().retire(n).await.unwrap_err(),
        ] {
            assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE", "{n:?}");
            assert_eq!(e.context.get("field"), Some(&json!("name")));
            assert!(e.message.contains("not \"actions\""));
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn crypts_gateway_refusals_pass_through() {
    for (status, code) in [
        (409, "K9O_CRYPT_ALREADY_EXISTS"),
        (409, "K9O_CRYPT_LIMIT_EXCEEDED"),
        (409, "K9O_CRYPT_RETIRED"),
        (404, "K9O_CRYPT_NOT_FOUND"),
        (403, "K9O_RBAC_DENY"),
        (403, "K9O_IAM_POLICY_DENIED"),
        (403, "K9O_ACCESS_DENIED"),
    ] {
        let (server, c) = setup(reply(
            status,
            json!({"code": code, "message": "no"}),
        ))
        .await;
        let e = c.crypto().crypts().retire("payments").await.unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(e.status, Some(status));
        let e = c.crypto().crypts().create("payments").await.unwrap_err();
        assert_eq!(e.code, code);
        assert_eq!(posts(&server).await.len(), 2, "not retried");
    }
}

#[tokio::test]
async fn crypts_malformed_answers_fail_closed() {
    let good = crypt_json("p", false);
    let mut no_name = good.clone();
    no_name.as_object_mut().unwrap().remove("name");
    let mut bad_status = good.clone();
    bad_status["status"] = json!("deleted");
    let mut no_created = good.clone();
    no_created.as_object_mut().unwrap().remove("createdAt");
    let mut bad_retired = good.clone();
    bad_retired["retiredAt"] = json!(5);
    let mut missing_retired = good.clone();
    missing_retired.as_object_mut().unwrap().remove("retiredAt");
    for bad in [no_name, bad_status, no_created, bad_retired, missing_retired, json!("x")] {
        let (_s, c) = setup(reply(201, json!({"data": bad, "meta": {}}))).await;
        let e = c.crypto().crypts().create("p").await.unwrap_err();
        assert_eq!(e.code, "K9O_INTERNAL_ERROR");
        assert!(e.message.starts_with("crypto.crypts.create: the gateway answered "), "{}", e.message);
        let (_s, c) = setup(reply(200, json!({"data": bad, "meta": {}}))).await;
        let e = c.crypto().crypts().retire("p").await.unwrap_err();
        assert!(e.message.starts_with("crypto.crypts.retire: the gateway answered "));
        let (_s, c) = setup(reply(200, json!({"data": {"crypts": [bad]}, "meta": {}}))).await;
        let e = c.crypto().crypts().list().await.unwrap_err();
        assert!(e.message.starts_with("crypto.crypts.list: the gateway answered "));
    }
    for d in [json!({}), json!({"crypts": "x"}), json!(null)] {
        let (_s, c) = setup(reply(200, json!({"data": d, "meta": {}}))).await;
        let e = c.crypto().crypts().list().await.unwrap_err();
        assert_eq!(e.code, "K9O_INTERNAL_ERROR");
        assert_eq!(e.message, "crypto.crypts.list: the gateway answered without crypts");
    }
}

// ---- the route named by the mode answer ----

#[tokio::test]
async fn byok_without_a_route_goes_to_the_client_core() {
    for m in ["BYOK", "HYOK"] {
        let (server, c) = setup_mode(mode_reply(m, "e2"), ok_batch()).await;
        let core = Arc::new(FakeCore::default());
        c.crypto().use_client_core(core.clone());
        c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
        assert_eq!(core.calls.lock().unwrap().len(), 1, "{m}");
        assert!(posts(&server).await.is_empty(), "{m}");
    }
}

#[tokio::test]
async fn byok_with_route_client_goes_to_the_client_core() {
    let (server, c) = setup_mode(route_reply("BYOK", "e2", json!("client")), ok_batch()).await;
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
    assert_eq!(core.calls.lock().unwrap().len(), 1);
    assert!(posts(&server).await.is_empty());
}

#[tokio::test]
async fn byok_with_route_server_sends_to_the_server_and_not_the_core() {
    for m in ["BYOK", "HYOK"] {
        let (server, c) = setup_mode(route_reply(m, "e7", json!("server")), ok_batch()).await;
        let core = Arc::new(FakeCore::default());
        c.crypto().use_client_core(core.clone());
        c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
        assert!(core.calls.lock().unwrap().is_empty(), "{m}");
        let (_, body) = only_body(&server).await;
        assert_eq!(body["custodyEpoch"], "e7", "{m}");
    }
}

#[tokio::test]
async fn an_unknown_route_is_refused_and_nothing_is_sent() {
    for (mode, route) in [
        ("BYOK", json!("bogus")),
        ("BYOK", json!(5)),
        ("BYOK", json!(123)),
        ("BYOK", json!(true)),
        ("BYOK", json!("Server")),
        ("BYOK", Value::Null),
        ("PLATFORM_MANAGED", json!("bogus")),
        ("PLATFORM_MANAGED", json!("")),
    ] {
        let reply = self::reply(
            200,
            json!({"data": {"custodyMode": mode, "epoch": "e2", "route": route}, "meta": {}}),
        );
        let (server, c) = setup_mode(reply, ok_batch()).await;
        let core = Arc::new(FakeCore::default());
        c.crypto().use_client_core(core.clone());
        let e = c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap_err();
        assert_eq!(e.code, "K9O_CRYPTO_CUSTODY_UNKNOWN", "{mode} {route}");
        assert_eq!(e.context["custodyMode"], mode);
        if let Some(r) = route.as_str() {
            assert_eq!(e.context["route"], r);
        } else {
            assert!(e.context.get("route").is_none());
        }
        assert!(posts(&server).await.is_empty(), "{mode} {route}");
        assert!(core.calls.lock().unwrap().is_empty(), "{mode} {route}");
    }
}

#[tokio::test]
async fn byok_route_server_without_an_epoch_is_refused() {
    let (server, c) = setup_mode(route_reply("BYOK", "", json!("server")), ok_batch()).await;
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    let e = c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap_err();
    assert_eq!(e.code, "K9O_CRYPTO_CUSTODY_UNKNOWN");
    assert!(posts(&server).await.is_empty());
    assert!(core.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn platform_managed_with_route_client_or_server_still_uses_the_server() {
    for r in [json!("server"), json!("client")] {
        let (server, c) = setup_mode(route_reply("PLATFORM_MANAGED", "e1", r), ok_batch()).await;
        c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
        assert_eq!(posts(&server).await.len(), 1);
    }
}

#[tokio::test]
async fn a_byok_route_is_read_fresh_on_every_encrypt() {
    let (server, c) = setup_mode(route_reply("BYOK", "e2", json!("server")), ok_batch()).await;
    for _ in 0..2 {
        c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
    }
    assert_eq!(mode_gets(&server).await, 2);
}

#[tokio::test]
async fn a_route_that_changes_between_two_encrypts_is_followed() {
    let server = MockServer::start().await;
    // First answer: your own key, client route. Every later answer: the server route.
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(route_reply("BYOK", "e2", json!("client")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/@portal/crypto/mode"))
        .respond_with(route_reply("BYOK", "e3", json!("server")))
        .mount(&server)
        .await;
    Mock::given(any()).respond_with(ok_batch()).mount(&server).await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        retry_config: Some(RetryConfig::default()),
        ..Default::default()
    })
    .unwrap();
    let core = Arc::new(FakeCore::default());
    c.crypto().use_client_core(core.clone());
    c.crypto().encrypt("k", &[PlainItem::text("x")]).await.unwrap();
    assert_eq!(core.calls.lock().unwrap().len(), 1);
    assert!(posts(&server).await.is_empty());
    c.crypto().encrypt("k", &[PlainItem::text("y")]).await.unwrap();
    assert_eq!(core.calls.lock().unwrap().len(), 1, "the second encrypt must not use the core");
    let (_, body) = only_body(&server).await;
    assert_eq!(body["custodyEpoch"], "e3");
}
