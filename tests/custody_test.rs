//! `custody().status()`: method, path, decoding of today's answer and of a hardware-module answer, and errors.

use k9o::{Client, ClientOpts};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(status: u16, body: Value, project: Option<&str>) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: project.map(Into::into),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

#[tokio::test]
async fn platform_managed_without_hsm_key() {
    let (server, c) = setup(
        200,
        json!({"data": {"mode": "PLATFORM_MANAGED", "attached": false, "provider": null, "lastJob": null}, "meta": {}}),
        Some("portal"),
    )
    .await;
    let s = c.custody().status().await.unwrap().data;
    assert_eq!(s.mode, "PLATFORM_MANAGED");
    assert!(!s.attached);
    assert!(s.provider.is_none() && s.last_job.is_none() && s.hsm.is_none());
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].method.as_str(), "GET");
    assert_eq!(reqs[0].url.path(), "/@portal/custody/status");
}

#[tokio::test]
async fn hyok_with_hsm() {
    let (_server, c) = setup(
        200,
        json!({"data": {"mode": "HYOK", "attached": true, "provider": "hsm",
            "lastJob": {"kind": "external_transition", "status": "completed", "completedAt": "2026-10-26T10:00:00Z"},
            "hsm": {"vendor": "aws-cloudhsm", "clusterId": "cluster-1234567890abcdef", "region": "de01",
                    "attestation": {"status": null, "verifiedAt": null}, "keys": null}}, "meta": {}}),
        Some("portal"),
    )
    .await;
    let s = c.custody().status().await.unwrap().data;
    assert_eq!(s.provider.as_deref(), Some("hsm"));
    let job = s.last_job.unwrap();
    assert_eq!(job.kind, "external_transition");
    assert_eq!(job.completed_at.as_deref(), Some("2026-10-26T10:00:00Z"));
    let hsm = s.hsm.unwrap();
    assert_eq!(hsm.vendor.as_deref(), Some("aws-cloudhsm"));
    assert_eq!(hsm.cluster_id.as_deref(), Some("cluster-1234567890abcdef"));
    assert_eq!(hsm.region.as_deref(), Some("de01"));
    assert!(hsm.attestation.status.is_none() && hsm.attestation.verified_at.is_none());
    assert!(hsm.keys.is_none());
}

#[tokio::test]
async fn gateway_errors_surface() {
    for (status, code) in [
        (403, "K9O_RBAC_DENY"),
        (404, "K9O_NOT_FOUND"),
        (502, "K9O_UPSTREAM_UNAVAILABLE"),
    ] {
        let (_s, c) = setup(
            status,
            json!({"code": code, "message": "m", "context": {}}),
            Some("portal"),
        )
        .await;
        let e = c.custody().status().await.unwrap_err();
        assert_eq!(e.code, code);
    }
}

#[tokio::test]
async fn requires_project() {
    let (server, c) = setup(200, json!({}), None).await;
    let e = c.custody().status().await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert!(server.received_requests().await.unwrap().is_empty());
}
