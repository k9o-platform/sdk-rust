//! `client.storage()` buckets and objects: verb, URL, body and query
//! of each operation, the multipart upload, the required `expires_in`, the
//! bound-project guard, and error passthrough.

use k9o::{
    Client, ClientOpts, CreateBucketOptions, SignedUrlOptions, UpdateBucketOptions, UploadOptions,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn client(server: &MockServer, slug: Option<&str>) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: slug.map(String::from),
        ..Default::default()
    })
    .unwrap()
}

async fn server_with(status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

async fn ok() -> MockServer {
    server_with(200, json!({"data": null, "meta": {}})).await
}

async fn ok_list() -> MockServer {
    server_with(200, json!({"data": [], "meta": {}})).await
}

async fn last(server: &MockServer) -> Request {
    server.received_requests().await.unwrap().pop().unwrap()
}

async fn count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// `METHOD /path?query` of the last request, raw (as sent on the wire).
async fn line(server: &MockServer) -> String {
    let r = last(server).await;
    let q = r.url.query().map(|q| format!("?{}", q)).unwrap_or_default();
    format!("{} {}{}", r.method.as_str(), r.url.path(), q)
}

const ROOT: &str = "/@member-portal/storage/buckets";

#[tokio::test]
async fn list_buckets_gets_root() {
    let s = ok_list().await;
    client(&s, Some("member-portal"))
        .storage()
        .list_buckets()
        .await
        .unwrap();
    assert_eq!(line(&s).await, format!("GET {}", ROOT));
}

#[tokio::test]
async fn get_bucket_encodes_name() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    let _ = c.storage().get_bucket("avatars").await;
    assert_eq!(line(&s).await, format!("GET {}/avatars", ROOT));
    let _ = c.storage().get_bucket("a/b c").await;
    assert_eq!(line(&s).await, format!("GET {}/a%2Fb%20c", ROOT));
}

#[tokio::test]
async fn create_bucket_posts_name_and_options_as_json() {
    let s = ok().await;
    let _ = client(&s, Some("member-portal"))
        .storage()
        .create_bucket(
            "avatars",
            CreateBucketOptions {
                public: Some(false),
                file_size_limit: Some(10_485_760),
                allowed_mime_types: Some(vec!["image/png".into()]),
                versioning: Some(true),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(line(&s).await, format!("POST {}", ROOT));
    let r = last(&s).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&r.body).unwrap(),
        json!({"name": "avatars", "public": false, "fileSizeLimit": 10485760,
               "allowedMimeTypes": ["image/png"], "versioning": true})
    );
    assert_eq!(
        r.headers.get("content-type").unwrap().to_str().unwrap(),
        "application/json"
    );
}

#[tokio::test]
async fn update_bucket_patches_only_what_is_given_and_null_clears() {
    let s = ok().await;
    let _ = client(&s, Some("member-portal"))
        .storage()
        .update_bucket(
            "avatars",
            UpdateBucketOptions {
                file_size_limit: Some(None),
                delete_after_days: Some(Some(30)),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(line(&s).await, format!("PATCH {}/avatars", ROOT));
    assert_eq!(
        serde_json::from_slice::<Value>(&last(&s).await.body).unwrap(),
        json!({"fileSizeLimit": null, "deleteAfterDays": 30})
    );
}

#[tokio::test]
async fn delete_bucket_sends_empty_true_only_when_asked() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    let _ = c.storage().delete_bucket("avatars", false).await;
    assert_eq!(line(&s).await, format!("DELETE {}/avatars", ROOT));
    let _ = c.storage().delete_bucket("avatars", true).await;
    assert_eq!(
        line(&s).await,
        format!("DELETE {}/avatars?empty=true", ROOT)
    );
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

#[tokio::test]
async fn upload_posts_multipart_with_all_parts_and_no_json_content_type() {
    let s = ok().await;
    let mut md = HashMap::new();
    md.insert("userId".to_string(), "u1".to_string());
    let _ = client(&s, Some("member-portal"))
        .storage()
        .from("avatars")
        .upload(
            "user/a.jpg",
            [1u8, 2, 3, 0xff],
            UploadOptions {
                content_type: Some("image/jpeg".into()),
                upsert: true,
                metadata: Some(md),
            },
        )
        .await;
    assert_eq!(line(&s).await, format!("POST {}/avatars/objects", ROOT));
    let r = last(&s).await;
    let ct = r.headers.get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    assert!(!ct.contains("json"));
    let boundary = ct.split("boundary=").nth(1).unwrap();
    let body = text(&r.body);
    assert!(body.starts_with(&format!("--{}\r\n", boundary)));
    assert!(body.ends_with(&format!("--{}--\r\n", boundary)));
    assert!(body.contains("name=\"file\"; filename=\"a.jpg\"\r\nContent-Type: image/jpeg\r\n"));
    // raw bytes appear verbatim between the headers and the next boundary
    let needle: Vec<u8> = [&b"\r\n\r\n"[..], &[1, 2, 3, 0xff], &b"\r\n--"[..]].concat();
    assert!(r.body.windows(needle.len()).any(|w| w == needle.as_slice()));
    assert!(body.contains("name=\"path\"\r\n\r\nuser/a.jpg\r\n"));
    assert!(body.contains("name=\"contentType\"\r\n\r\nimage/jpeg\r\n"));
    assert!(body.contains("name=\"upsert\"\r\n\r\ntrue\r\n"));
    assert!(body.contains("name=\"metadata\"\r\n\r\n{\"userId\":\"u1\"}\r\n"));
}

#[tokio::test]
async fn upload_defaults_upsert_false_and_accepts_a_string() {
    let s = ok().await;
    let _ = client(&s, Some("member-portal"))
        .storage()
        .from("docs")
        .upload("a.txt", "héllo", UploadOptions::default())
        .await;
    let r = last(&s).await;
    let body = text(&r.body);
    assert!(body.contains("name=\"upsert\"\r\n\r\nfalse\r\n"));
    assert!(!body.contains("name=\"contentType\""));
    assert!(!body.contains("name=\"metadata\""));
    let needle = "\r\n\r\nhéllo\r\n--".as_bytes();
    assert!(r.body.windows(needle.len()).any(|w| w == needle));
}

#[tokio::test]
async fn create_signed_url_posts_body() {
    let s = ok().await;
    let _ = client(&s, Some("member-portal"))
        .storage()
        .from("avatars")
        .create_signed_url(
            "a.jpg",
            SignedUrlOptions {
                expires_in: 300,
                download: Some(true),
            },
        )
        .await;
    assert_eq!(
        line(&s).await,
        format!("POST {}/avatars/objects/sign", ROOT)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&last(&s).await.body).unwrap(),
        json!({"path": "a.jpg", "expiresIn": 300, "download": true})
    );
}

#[tokio::test]
async fn expires_in_bounds_refused_before_any_request() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    let h = c.storage().from("avatars");
    for bad in [0i64, -1, 604_801, i64::MAX] {
        let err = h
            .create_signed_url(
                "a.jpg",
                SignedUrlOptions {
                    expires_in: bad,
                    download: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "K9O_INVALID_FIELD_VALUE", "{bad}");
    }
    assert_eq!(count(&s).await, 0, "no request on refusal");
    for good in [1i64, 604_800] {
        h.create_signed_url(
            "a.jpg",
            SignedUrlOptions {
                expires_in: good,
                download: None,
            },
        )
        .await
        .ok();
        assert_eq!(
            serde_json::from_slice::<Value>(&last(&s).await.body).unwrap()["expiresIn"],
            good
        );
    }
    assert_eq!(count(&s).await, 2);
}

#[tokio::test]
async fn list_sends_prefix_only_when_given_and_encodes_it() {
    let s = ok_list().await;
    let c = client(&s, Some("member-portal"));
    let h = c.storage().from("avatars");
    h.list(None).await.unwrap();
    assert_eq!(line(&s).await, format!("GET {}/avatars/objects", ROOT));
    h.list(Some("user/")).await.unwrap();
    assert_eq!(
        line(&s).await,
        format!("GET {}/avatars/objects?prefix=user%2F", ROOT)
    );
    h.list(Some("a b&c=d")).await.unwrap();
    assert_eq!(
        line(&s).await,
        format!("GET {}/avatars/objects?prefix=a+b%26c%3Dd", ROOT)
    );
}

#[tokio::test]
async fn remove_deletes_with_encoded_path_query() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    let h = c.storage().from("avatars");
    let _ = h.remove("a b&c.jpg").await;
    assert_eq!(
        line(&s).await,
        format!("DELETE {}/avatars/objects?path=a+b%26c.jpg", ROOT)
    );
    // `..` and a leading `/` are not rewritten, only encoded
    let _ = h.remove("/../x").await;
    assert_eq!(
        line(&s).await,
        format!("DELETE {}/avatars/objects?path=%2F..%2Fx", ROOT)
    );
}

#[tokio::test]
async fn bucket_name_with_traversal_is_encoded_in_the_path() {
    let s = ok_list().await;
    let _ = client(&s, Some("member-portal"))
        .storage()
        .from("../x")
        .list(None)
        .await;
    assert_eq!(line(&s).await, format!("GET {}/..%2Fx/objects", ROOT));
}

#[tokio::test]
async fn empty_path_or_bucket_refused_before_any_request() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    let h = c.storage().from("avatars");
    let errs = vec![
        h.upload("", "x", UploadOptions::default())
            .await
            .unwrap_err(),
        h.remove("").await.unwrap_err(),
        h.create_signed_url(
            "",
            SignedUrlOptions {
                expires_in: 60,
                download: None,
            },
        )
        .await
        .unwrap_err(),
        c.storage().get_bucket("").await.unwrap_err(),
    ];
    for e in errs {
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    }
    assert_eq!(count(&s).await, 0);
}

#[tokio::test]
async fn dot_and_dotdot_bucket_names_refused_before_any_request() {
    let s = ok().await;
    let c = client(&s, Some("member-portal"));
    for name in [".", ".."] {
        let h = c.storage().from(name);
        let errs = vec![
            c.storage().get_bucket(name).await.unwrap_err(),
            c.storage()
                .update_bucket(name, UpdateBucketOptions::default())
                .await
                .unwrap_err(),
            c.storage().delete_bucket(name, false).await.unwrap_err(),
            h.upload("a.txt", "x", UploadOptions::default())
                .await
                .unwrap_err(),
            h.create_signed_url(
                "a.txt",
                SignedUrlOptions {
                    expires_in: 60,
                    download: None,
                },
            )
            .await
            .unwrap_err(),
            h.list(None).await.unwrap_err(),
            h.remove("a.txt").await.unwrap_err(),
        ];
        for e in errs {
            assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
            assert_eq!(
                e.context.get("reason").and_then(|v| v.as_str()),
                Some("dot_segment")
            );
            assert_eq!(
                e.context.get("field").and_then(|v| v.as_str()),
                Some("bucket")
            );
        }
    }
    assert_eq!(count(&s).await, 0);
    // positive control: an ordinary name still goes out
    let _ = c.storage().get_bucket("avatars").await;
    assert_eq!(count(&s).await, 1);
}

#[tokio::test]
async fn missing_project_slug_refused() {
    let s = ok().await;
    let c = client(&s, None);
    let e1 = c.storage().list_buckets().await.unwrap_err();
    let e2 = c.storage().from("a").list(None).await.unwrap_err();
    let e3 = c
        .storage()
        .from("a")
        .upload("p", "x", UploadOptions::default())
        .await
        .unwrap_err();
    for e in [e1, e2, e3] {
        assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    }
    assert_eq!(count(&s).await, 0);
}

#[tokio::test]
async fn gateway_4xx_passes_through_without_object_bytes() {
    let s = server_with(
        409,
        json!({"code": "bucket_not_empty", "message": "bucket is not empty",
               "context": {}, "recommendation": null}),
    )
    .await;
    let c = client(&s, Some("member-portal"));
    let err = c
        .storage()
        .delete_bucket("avatars", false)
        .await
        .unwrap_err();
    assert_eq!(err.code, "bucket_not_empty");
    assert_eq!(err.status, Some(409));

    let err = c
        .storage()
        .from("avatars")
        .upload("a.txt", "TOP-SECRET-BYTES", UploadOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.status, Some(409));
    let shown = format!("{:?} {}", err, err);
    assert!(!shown.contains("TOP-SECRET-BYTES"));
}

#[tokio::test]
async fn invalid_bucket_name_error_passes_through() {
    let s = server_with(
        400,
        json!({"code": "K9O_INVALID_BUCKET_NAME", "message": "bad name",
               "context": {}, "recommendation": null}),
    )
    .await;
    let err = client(&s, Some("member-portal"))
        .storage()
        .create_bucket("Bad_Name", CreateBucketOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_INVALID_BUCKET_NAME");
}
