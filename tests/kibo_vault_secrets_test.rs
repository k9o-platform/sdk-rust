//! Wire-shape tests for `client.kibo().vault().secrets().*` — the
//! full family: list / get / create / reveal_challenge / reveal / update /
//! rotate / move / delete.
//!
//! Mirrors `packages/kibo/test/secrets.test.ts` (the TS reference): verifies
//! URL/query/method/body against the live contract (not just the documented
//! OpenAPI schema, which is inaccurate for `update`/`rotate`/`move`'s
//! response shape), including:
//!  - `region` optional on every op (sent only when given)
//!  - `get` reads the metadata through `list` by exact-name prefix
//!  - `value` base64-encoded on write, base64-decoded on reveal
//!  - the `X-K9O-Reveal-Challenge` header on `reveal`
//!  - the hierarchical `name` (e.g. `db/pw`) percent-encoded as a single
//!    path segment (`db%2Fpw`)
//!
//! All against a `wiremock` gateway — no real network calls.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use k9o::{
    Client, ClientOpts, CreateSecretInput, ListSecretsInput, SecretVisibility, UpdateSecretInput,
};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn project_client(server: &MockServer) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("member-portal".into()),
        ..Default::default()
    })
    .unwrap()
}

fn summary_json() -> serde_json::Value {
    json!({
        "id": "sec_abc", "name": "db/pw", "ownerId": "usr_1",
        "visibility": {"kind": "private"}, "version": 1, "kid": "vkey_1",
        "createdAt": "t", "createdBy": "usr_1", "updatedAt": "t", "updatedBy": "usr_1",
        "rotatedAt": null, "retiredAt": null, "metadata": {},
    })
}

#[tokio::test]
async fn list_gets_secrets_with_prefix_cursor_limit_and_no_region() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@member-portal/vault/secrets"))
        .and(query_param("prefix", "db/"))
        .and(query_param("limit", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"secrets": [summary_json()], "nextCursor": null},
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .list(ListSecretsInput {
            prefix: Some("db/"),
            limit: Some(10),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(resp.data.secrets[0].id, "sec_abc");
    assert!(resp.data.next_cursor.is_none());
}

#[tokio::test]
async fn list_does_not_require_project_context_error_shape_but_does_require_project() {
    // list still requires project context (member-plane) — just not `region`.
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c
        .kibo()
        .vault()
        .secrets()
        .list(ListSecretsInput::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}

#[tokio::test]
async fn get_lists_by_exact_name_prefix_and_picks_the_exact_match() {
    let server = MockServer::start().await;
    let mut other = summary_json();
    other["name"] = json!("db/pw-2");
    Mock::given(method("GET"))
        .and(path("/@member-portal/vault/secrets"))
        .and(query_param("prefix", "db/pw"))
        .and(query_param("limit", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"secrets": [other, summary_json()], "nextCursor": null},
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c.kibo().vault().secrets().get("db/pw", "").await.unwrap();
    assert_eq!(resp.data.name, "db/pw");
    assert_eq!(resp.data.owner_id, "usr_1");
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].url.query_pairs().all(|(k, _)| k != "region"));
}

#[tokio::test]
async fn get_not_found_is_a_404_kerror() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@member-portal/vault/secrets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"secrets": [], "nextCursor": null},
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = project_client(&server);
    let e = c
        .kibo()
        .vault()
        .secrets()
        .get("db/pw", "")
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_NOT_FOUND");
    assert_eq!(e.status, Some(404));
}

#[tokio::test]
async fn create_posts_with_region_query_and_base64_encoded_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("region", "us01"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["value"].as_str().unwrap(), BASE64.encode(b"hunter2"));
            assert_ne!(body["value"].as_str().unwrap(), "hunter2");
            ResponseTemplate::new(200).set_body_json(json!({"data": summary_json(), "meta": {}}))
        })
        .mount(&server)
        .await;

    let c = project_client(&server);
    c.kibo()
        .vault()
        .secrets()
        .create(CreateSecretInput {
            name: "db/pw",
            value: "hunter2",
            region: "us01",
            visibility: Some(SecretVisibility::Private),
            metadata: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn reveal_challenge_posts_op_reveal_challenge_with_region() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("op", "reveal-challenge"))
        .and(query_param("region", "us01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"challenge": "rvc_1", "expiresAt": "t"},
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .reveal_challenge("db/pw", "us01")
        .await
        .unwrap();
    assert_eq!(resp.data.challenge, "rvc_1");
}

#[tokio::test]
async fn reveal_gets_with_challenge_header_and_decodes_base64_value_to_plaintext() {
    let server = MockServer::start().await;
    let wire_value = BASE64.encode(b"hunter2");
    Mock::given(method("GET"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("region", "us01"))
        .and(header("X-K9O-Reveal-Challenge", "rvc_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "id": "sec_abc", "name": "db/pw", "version": 1, "value": wire_value,
                "revealedAt": "t",
                "attestationTier": {"urn": "u", "operator": "platform", "verified": true},
            },
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .reveal("db/pw", "us01", "rvc_1")
        .await
        .unwrap();
    assert_eq!(resp.data.value, "hunter2");
    assert_eq!(resp.data.attestation_tier.operator, "platform");
    assert!(resp.data.attestation_tier.verified);
}

#[tokio::test]
async fn update_puts_with_region_query_and_base64_value_returns_narrow_shape() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("region", "us01"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["value"].as_str().unwrap(), BASE64.encode(b"newpw"));
            ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sec_abc", "name": "db/pw", "version": 2},
                "meta": {}
            }))
        })
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .update(
            "db/pw",
            UpdateSecretInput {
                region: "us01",
                value: "newpw",
                metadata: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(resp.data.version, 2);
}

#[tokio::test]
async fn rotate_posts_op_rotate_with_no_body_and_returns_undocumented_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("op", "rotate"))
        .and(query_param("region", "us01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"id": "sec_abc", "name": "db/pw", "version": 2, "rotatedAt": "t", "ledgerSequenceNumber": 2},
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .rotate("db/pw", "us01")
        .await
        .unwrap();
    assert_eq!(resp.data.ledger_sequence_number, 2);
}

#[tokio::test]
async fn move_posts_op_move_with_new_path_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("op", "move"))
        .and(query_param("region", "us01"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body, json!({"newPath": "db/pw2"}));
            ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sec_abc", "name": "db/pw2", "event": "SECRET_PATH_CHANGED"},
                "meta": {}
            }))
        })
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .r#move("db/pw", "us01", "db/pw2")
        .await
        .unwrap();
    assert_eq!(resp.data.event, "SECRET_PATH_CHANGED");
}

#[tokio::test]
async fn delete_sends_region_query_and_handles_204() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/@member-portal/vault/secrets/db%2Fpw"))
        .and(query_param("region", "us01"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = project_client(&server);
    let resp = c
        .kibo()
        .vault()
        .secrets()
        .delete("db/pw", "us01")
        .await
        .unwrap();
    assert!(resp.data.is_null());
}

#[tokio::test]
async fn every_named_secret_op_requires_project_context() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    let secrets = c.kibo().vault().secrets();
    assert_eq!(
        secrets.get("db/pw", "us01").await.unwrap_err().code,
        "K9O_MISSING_PROJECT_CONTEXT"
    );
    assert_eq!(
        secrets.rotate("db/pw", "us01").await.unwrap_err().code,
        "K9O_MISSING_PROJECT_CONTEXT"
    );
}
