use k9o::{Client, ClientOpts};
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Debug, Deserialize, PartialEq)]
struct Post {
    id: String,
    title: String,
}

#[tokio::test]
async fn client_constructs_with_minimal_opts() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(c.base_url(), "https://api.k9o.io");
}

#[tokio::test]
async fn client_rejects_path_bearing_base_url() {
    let e = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io/v2".into()),
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
}

#[tokio::test]
async fn projects_list_routes_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[],"meta":{}})),
        )
        .mount(&server)
        .await;

    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        org_slug: Some("acme".into()),
        ..Default::default()
    })
    .unwrap();
    let resp = c.projects().list().await.unwrap();
    assert!(resp.data.is_empty());
}

#[tokio::test]
async fn projects_create_validates_slug() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c
        .projects()
        .create(k9o::CreateProjectInput {
            slug: Some("Bad-Slug"),
            display_name: "x",
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
}

#[tokio::test]
async fn projects_get_routes_to_project_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects/member-portal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"id":"p","slug":"member-portal","displayName":"MP","custodyMode":"PLATFORM_MANAGED","createdAt":"x","orgId":"o"},
            "meta": {}
        })))
        .mount(&server)
        .await;

    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        ..Default::default()
    })
    .unwrap();
    let resp = c.projects().get("member-portal").await.unwrap();
    assert_eq!(resp.data.slug, "member-portal");
    assert_eq!(resp.data.display_name, "MP");
}

#[tokio::test]
async fn data_select_routes_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@member-portal/data/posts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data":[{"id":"r1","title":"hi"}],
            "meta":{"cursor":{"next":"abc"}}
        })))
        .mount(&server)
        .await;

    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("member-portal".into()),
        ..Default::default()
    })
    .unwrap();
    let resp = c
        .from("posts")
        .select()
        .eq("status", "published")
        .gt("views", 100)
        .limit(20)
        .execute::<Post>()
        .await
        .unwrap();
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].title, "hi");
    assert_eq!(
        resp.meta.cursor.as_ref().unwrap().next.as_deref(),
        Some("abc")
    );
}

#[tokio::test]
async fn data_select_requires_project_slug() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c
        .from("posts")
        .select()
        .execute::<Post>()
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}

#[tokio::test]
async fn data_update_requires_id_filter() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        project_slug: Some("member-portal".into()),
        ..Default::default()
    })
    .unwrap();
    let patch = serde_json::json!({"title": "x"});
    let e = c
        .from("posts")
        .update(patch)
        .execute::<serde_json::Value>()
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
}

#[tokio::test]
async fn error_handling_rehydrates_kerror() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "code": "K9O_TIER_CAP_HIT",
            "message": "cap exceeded",
            "context": {"dimension": "mau", "limit": 1000},
            "recommendation": "upgrade to Ascent",
            "requestId": "req_x"
        })))
        .mount(&server)
        .await;

    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        org_slug: Some("acme".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c.projects().list().await.unwrap_err();
    assert_eq!(e.code, "K9O_TIER_CAP_HIT");
    assert_eq!(e.status, Some(429));
    assert_eq!(e.recommendation.as_deref(), Some("upgrade to Ascent"));
    assert_eq!(e.request_id.as_deref(), Some("req_x"));
}

#[tokio::test]
async fn gate_sign_in_routes_correctly() {
    let server = MockServer::start().await;
    // Sign-in asks the locate door first; this test is about the call after it (src/gate.rs covers the door).
    Mock::given(method("POST"))
        .and(path("/@member-portal/gate/locate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data":{"region":"de01","origin":null},
            "meta":{}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/@member-portal/gate/signin"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data":{"user":{"id":"eu_x"},"session":{}},
            "meta":{}
        })))
        .mount(&server)
        .await;

    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("member-portal".into()),
        ..Default::default()
    })
    .unwrap();
    let resp = c
        .gate()
        .sign_in(k9o::SignInInput {
            email: "x@y.com",
            password: "z",
        })
        .await
        .unwrap();
    assert!(resp.data.get("user").is_some());
}

#[tokio::test]
async fn gate_requires_project_slug() {
    let c = Client::new(ClientOpts {
        base_url: Some("https://api.k9o.io".into()),
        ..Default::default()
    })
    .unwrap();
    let e = c.gate().whoami().await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}

async fn projects_list_error(header: Option<(&str, &str)>, body: serde_json::Value) -> k9o::KError {
    let server = MockServer::start().await;
    let mut tpl = ResponseTemplate::new(404).set_body_json(body);
    if let Some((k, v)) = header {
        tpl = tpl.insert_header(k, v);
    }
    Mock::given(method("GET"))
        .and(path("/projects"))
        .respond_with(tpl)
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        org_slug: Some("acme".into()),
        ..Default::default()
    })
    .unwrap();
    c.projects().list().await.unwrap_err()
}

fn not_found_body() -> serde_json::Value {
    serde_json::json!({"code":"K9O_NOT_FOUND","message":"nope","context":{}})
}

#[tokio::test]
async fn error_request_id_comes_from_the_header() {
    let e = projects_list_error(Some(("x-k9o-request-id", "req_abc")), not_found_body()).await;
    assert_eq!(e.status, Some(404));
    assert_eq!(e.request_id.as_deref(), Some("req_abc"));
}

#[tokio::test]
async fn error_request_id_body_value_wins_over_header() {
    let mut body = not_found_body();
    body["requestId"] = serde_json::json!("req_body");
    let e = projects_list_error(Some(("x-k9o-request-id", "req_hdr")), body).await;
    assert_eq!(e.request_id.as_deref(), Some("req_body"));
}

#[tokio::test]
async fn error_request_id_is_none_without_header_or_body_value() {
    let e = projects_list_error(None, not_found_body()).await;
    assert_eq!(e.request_id, None);
}

#[tokio::test]
async fn error_request_id_header_name_is_case_insensitive() {
    let e = projects_list_error(Some(("X-K9O-Request-Id", "req_abc")), not_found_body()).await;
    assert_eq!(e.request_id.as_deref(), Some("req_abc"));
}
