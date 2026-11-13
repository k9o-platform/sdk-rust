//! Datasets: `analytics()` and `rollups()` over a stubbed transport. Every route's method, path, query, body
//! and decoding, the checks made before sending, and the error answers.

use k9o::{
    Client, ClientOpts, CreateDatasetInput, CreateDatasetScheduleInput, CreateRollupInput,
    DatasetColumn, DatasetUploadOptions, DatasetWriteOptions, OnError, ReadOptions, ResidencyClass,
    RollupMeasure, RollupReadOptions, StorageTarget, UploadFormat,
};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(status: u16, body: Value) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

async fn only(server: &MockServer) -> wiremock::Request {
    let mut r = server.received_requests().await.unwrap();
    assert_eq!(r.len(), 1, "exactly one request");
    r.remove(0)
}

fn ok(data: Value) -> Value {
    json!({ "data": data, "meta": {} })
}

#[tokio::test]
async fn list_gets_the_datasets() {
    let (server, c) = setup(
        200,
        ok(json!([{
            "slug": "orders", "displayName": "Orders", "residencyClass": "shared",
            "storageTarget": {"kind": "managed"}, "createdAt": "2026-10-01T10:00:00Z",
            "partitionBy": ["region"], "schema": [{"name": "id", "type": "string"}]
        }])),
    )
    .await;
    let r = c.analytics().list().await.unwrap().data;
    assert_eq!(r[0].slug, "orders");
    assert_eq!(r[0].display_name.as_deref(), Some("Orders"));
    assert_eq!(r[0].residency_class.as_deref(), Some("shared"));
    assert_eq!(r[0].extra["partitionBy"], json!(["region"]));
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "GET");
    assert_eq!(req.url.path(), "/@portal/analytics");
}

#[tokio::test]
async fn query_posts_sql_and_dataset_list() {
    let (server, c) = setup(200, ok(json!([{"n": 3}]))).await;
    let r = c
        .analytics()
        .query("SELECT count(*) AS n FROM dataset.orders", &["orders"])
        .await
        .unwrap();
    assert_eq!(r.data[0]["n"], 3);
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/@portal/analytics/query");
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({"sql": "SELECT count(*) AS n FROM dataset.orders", "datasets": ["orders"]})
    );
}

#[tokio::test]
async fn query_checks_before_sending() {
    let (server, c) = setup(200, ok(json!([]))).await;
    assert!(c.analytics().query("  ", &["orders"]).await.is_err());
    assert!(c.analytics().query("SELECT 1", &[]).await.is_err());
    let many: Vec<&str> = vec!["a"; 65];
    assert!(c.analytics().query("SELECT 1", &many).await.is_err());
    let long = "x".repeat(100_001);
    assert!(c.analytics().query(&long, &["a"]).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn create_posts_the_definition_at_the_slug() {
    let (server, c) = setup(
        201,
        ok(json!({"slug": "orders", "displayName": "Orders", "endpoints": {"read": "/x"}})),
    )
    .await;
    let input = CreateDatasetInput {
        display_name: "Orders".into(),
        columns: vec![
            DatasetColumn::new("id", "string"),
            DatasetColumn::new("created_at", "timestamp"),
        ],
        partition_by: Some(vec!["created_at:day".into()]),
        residency_class: ResidencyClass::Shared,
        storage_target: Some(StorageTarget::Byos {
            binding_id: "bind_1".into(),
        }),
    };
    let d = c.analytics().create("orders", &input).await.unwrap().data;
    assert_eq!(d.slug, "orders");
    assert_eq!(d.extra["endpoints"]["read"], "/x");
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/@portal/analytics/orders");
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({
            "displayName": "Orders",
            "columns": [{"name": "id", "type": "string"}, {"name": "created_at", "type": "timestamp"}],
            "partitionBy": ["created_at:day"],
            "residencyClass": "shared",
            "storageTarget": {"kind": "byos", "bindingId": "bind_1"}
        })
    );
}

#[tokio::test]
async fn create_omits_optional_members_and_per_subject_is_hyphenated() {
    let (server, c) = setup(201, ok(json!({"slug": "orders"}))).await;
    let input = CreateDatasetInput {
        display_name: "Orders".into(),
        columns: vec![DatasetColumn::new("id", "string")],
        partition_by: None,
        residency_class: ResidencyClass::PerSubject,
        storage_target: Some(StorageTarget::Managed),
    };
    c.analytics().create("orders", &input).await.unwrap();
    let body: Value = serde_json::from_slice(&only(&server).await.body).unwrap();
    assert_eq!(body["residencyClass"], "per-subject");
    assert_eq!(body["storageTarget"], json!({"kind": "managed"}));
    assert!(body.get("partitionBy").is_none());
}

#[tokio::test]
async fn create_checks_before_sending() {
    let (server, c) = setup(201, ok(json!({"slug": "x"}))).await;
    let base = CreateDatasetInput {
        display_name: "Orders".into(),
        columns: vec![DatasetColumn::new("id", "string")],
        partition_by: None,
        residency_class: ResidencyClass::Shared,
        storage_target: None,
    };
    let mut no_cols = base.clone();
    no_cols.columns.clear();
    assert!(c.analytics().create("orders", &no_cols).await.is_err());
    let mut no_name = base.clone();
    no_name.display_name.clear();
    assert!(c.analytics().create("orders", &no_name).await.is_err());
    let mut many_keys = base.clone();
    many_keys.partition_by = Some(vec!["a".into(); 17]);
    assert!(c.analytics().create("orders", &many_keys).await.is_err());
    assert!(c.analytics().create("", &base).await.is_err());
    assert!(c.analytics().create("..", &base).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn get_and_delete_use_the_dataset_path() {
    let (server, c) = setup(200, ok(json!({"slug": "orders"}))).await;
    c.analytics().get("orders").await.unwrap();
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/analytics/orders")
    );

    let (server, c) = setup(200, ok(json!({"slug": "orders", "status": "retired"}))).await;
    let r = c.analytics().delete("orders").await.unwrap().data;
    assert_eq!((r.slug.as_str(), r.status.as_str()), ("orders", "retired"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("DELETE", "/@portal/analytics/orders")
    );
}

#[tokio::test]
async fn a_slug_is_encoded_not_concatenated() {
    let (server, c) = setup(200, ok(json!({"slug": "x"}))).await;
    c.analytics().get("a/b?c").await.unwrap();
    let req = only(&server).await;
    assert_eq!(req.url.path(), "/@portal/analytics/a%2Fb%3Fc");
    assert!(req.url.query().is_none());
}

#[tokio::test]
async fn dataset_not_found_is_a_typed_error() {
    let (_s, c) = setup(
        404,
        json!({"code": "K9O_DATASET_NOT_FOUND", "message": "no such dataset"}),
    )
    .await;
    let e = c.analytics().get("nope").await.unwrap_err();
    assert_eq!(e.code, "K9O_DATASET_NOT_FOUND");
    assert_eq!(e.status, Some(404));
}

#[tokio::test]
async fn materialize_takes_exactly_one_source() {
    let (server, c) = setup(
        200,
        ok(json!({"rowsRead": 10, "ingested": 9, "rejected": 1, "partitions": ["region=de"]})),
    )
    .await;
    let r = c
        .analytics()
        .materialize("orders", Some("orders_raw"), None)
        .await
        .unwrap()
        .data;
    assert_eq!(
        (r.rows_read, r.ingested, r.rejected),
        (Some(10), Some(9), Some(1))
    );
    assert_eq!(r.extra["partitions"], json!(["region=de"]));
    let req = only(&server).await;
    assert_eq!(req.url.path(), "/@portal/analytics/orders/materialize");
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body, json!({"fromCollection": "orders_raw"}));

    c.analytics()
        .materialize("orders", None, Some("stq_1"))
        .await
        .unwrap();
    let all = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&all[1].body).unwrap();
    assert_eq!(body, json!({"fromQuery": "stq_1"}));

    assert!(c
        .analytics()
        .materialize("orders", None, None)
        .await
        .is_err());
    assert!(c
        .analytics()
        .materialize("orders", Some("a"), Some("b"))
        .await
        .is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn read_posts_the_page_request_and_returns_the_cursor() {
    let (server, c) = setup(
        200,
        json!({
            "data": [{"_k9o_row_id": "r1", "id": "a"}, {"_k9o_row_id": "r2", "id": "b"}],
            "meta": {"cursor": {"next": "cur_2"}, "schema": [{"name": "id", "type": "string"}]}
        }),
    )
    .await;
    let r = c
        .analytics()
        .read(
            "orders",
            ReadOptions {
                partition_filter: Some(json!({"region": "de"})),
                cursor: Some("cur_1".into()),
                limit: Some(2),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.data.len(), 2);
    assert_eq!(r.data[0]["_k9o_row_id"], "r1");
    assert_eq!(r.meta.cursor.unwrap().next.as_deref(), Some("cur_2"));
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/@portal/analytics/orders/read");
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({"partitionFilter": {"region": "de"}, "cursor": "cur_1", "limit": 2})
    );
}

#[tokio::test]
async fn read_without_options_sends_an_empty_object() {
    let (server, c) = setup(200, ok(json!([]))).await;
    c.analytics()
        .read("orders", ReadOptions::default())
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&only(&server).await.body).unwrap();
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn write_sends_ndjson_and_on_error() {
    let (server, c) = setup(
        200,
        ok(json!({"ingested": 2, "skipped": 1, "skippedDetails": [{"row": 3, "reason": "type"}], "bytes": 40})),
    )
    .await;
    let rows = vec![json!({"id": "a"}), json!({"id": "b"})];
    let r = c
        .analytics()
        .write(
            "orders",
            &rows,
            DatasetWriteOptions {
                on_error: Some(OnError::Reject),
            },
        )
        .await
        .unwrap()
        .data;
    assert_eq!((r.ingested, r.skipped), (Some(2), Some(1)));
    assert_eq!(r.skipped_details.len(), 1);
    assert_eq!(r.extra["bytes"], 40);
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/@portal/analytics/orders/write");
    assert_eq!(req.url.query(), Some("onError=reject"));
    assert_eq!(
        String::from_utf8(req.body.clone()).unwrap(),
        "{\"id\":\"a\"}\n{\"id\":\"b\"}\n"
    );
    assert!(req
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("ndjson"));
    assert!(c
        .analytics()
        .write("orders", &[], DatasetWriteOptions::default())
        .await
        .is_err());
}

#[tokio::test]
async fn upload_raw_body_with_query() {
    let (server, c) = setup(200, ok(json!({"ingested": 3, "skipped": 0}))).await;
    let mut o = DatasetUploadOptions::new(UploadFormat::Csv);
    o.dry_run = true;
    o.on_error = Some(OnError::Skip);
    o.delimiter = Some(";".into());
    o.encoding = Some("utf-8".into());
    o.has_header = Some(true);
    let r = c
        .analytics()
        .upload("orders", "id;name\n1;Alice\n", o)
        .await
        .unwrap()
        .data;
    assert_eq!(r.ingested, Some(3));
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "POST");
    assert_eq!(req.url.path(), "/@portal/analytics/orders/upload");
    let pairs: Vec<(String, String)> = req
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("format"), Some("csv"));
    assert_eq!(get("dryRun"), Some("true"));
    assert_eq!(get("onError"), Some("skip"));
    assert_eq!(get("delimiter"), Some(";"));
    assert_eq!(get("encoding"), Some("utf-8"));
    assert_eq!(get("hasHeader"), Some("true"));
    assert_eq!(req.headers.get("content-type").unwrap(), "text/csv");
    assert_eq!(req.body, b"id;name\n1;Alice\n");
}

#[tokio::test]
async fn upload_with_a_manifest_is_multipart() {
    let (server, c) = setup(200, ok(json!({"ingested": 1}))).await;
    let mut o = DatasetUploadOptions::new(UploadFormat::Json);
    o.manifest = Some(
        json!({"displayName": "Orders", "columns": [{"name": "id", "type": "string"}], "residencyClass": "shared"}),
    );
    o.file_name = Some("orders.json".into());
    c.analytics()
        .upload("orders", br#"[{"id":"a"}]"#, o)
        .await
        .unwrap();
    let req = only(&server).await;
    assert_eq!(req.url.query(), Some("format=json"));
    let ct = req
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8(req.body.clone()).unwrap();
    assert!(body.contains("name=\"file\"; filename=\"orders.json\""));
    assert!(body.contains("Content-Type: application/json"));
    assert!(body.contains("[{\"id\":\"a\"}]"));
    assert!(body.contains("name=\"manifest\""));
    assert!(body.contains("\"residencyClass\":\"shared\""));
}

#[tokio::test]
async fn upload_checks_size_before_sending() {
    let (server, c) = setup(200, ok(json!({}))).await;
    let big = vec![b'a'; 25 * 1024 * 1024 + 1];
    assert!(c
        .analytics()
        .upload("orders", big, DatasetUploadOptions::new(UploadFormat::Csv))
        .await
        .is_err());
    assert!(c
        .analytics()
        .upload(
            "orders",
            Vec::<u8>::new(),
            DatasetUploadOptions::new(UploadFormat::Csv)
        )
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn upload_429_is_a_typed_error() {
    let (_s, c) = setup(
        429,
        json!({"code": "K9O_DATASET_LIMIT_EXCEEDED", "message": "200 datasets"}),
    )
    .await;
    let e = c
        .analytics()
        .upload(
            "new",
            "a\n1\n",
            DatasetUploadOptions::new(UploadFormat::Csv),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_DATASET_LIMIT_EXCEEDED");
    assert_eq!(e.status, Some(429));
}

#[tokio::test]
async fn download_original_returns_the_bytes_not_an_envelope() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = vec![0, 1, 2, 255, 254, b'\n'];
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(payload.clone()),
        )
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    let f = c.analytics().download_original("orders").await.unwrap();
    assert_eq!(f.bytes, payload);
    assert_eq!(f.content_type.as_deref(), Some("application/octet-stream"));
    let req = only(&server).await;
    assert_eq!(req.method.as_str(), "GET");
    assert_eq!(req.url.path(), "/@portal/analytics/orders/upload/original");
}

#[tokio::test]
async fn download_original_404_is_a_typed_error() {
    let (_s, c) = setup(
        404,
        json!({"code": "K9O_NOT_FOUND", "message": "none stored"}),
    )
    .await;
    let e = c.analytics().download_original("orders").await.unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_NOT_FOUND", Some(404)));
}

fn rollup_input() -> CreateRollupInput {
    CreateRollupInput {
        slug: "orders_hourly".into(),
        source: "orders".into(),
        dimensions: vec!["city".into()],
        measures: vec![
            RollupMeasure {
                name: "n".into(),
                agg: "count".into(),
                column: None,
            },
            RollupMeasure {
                name: "total".into(),
                agg: "sum".into(),
                column: Some("amount".into()),
            },
        ],
        window_seconds: 3600,
        reattest_interval_days: Some(30),
        subject_field: None,
        non_personal: Some(true),
    }
}

#[tokio::test]
async fn rollup_create_list_delete_reattest() {
    let (server, c) = setup(
        201,
        ok(
            json!({"id": "rol_1", "slug": "orders_hourly", "attestedAt": "2026-10-27T10:00:00Z",
                   "expiresAt": "2026-11-26T10:00:00Z", "attestationStatus": "active",
                   "endpoints": {"read": "/r", "subscribe": "/s"}}),
        ),
    )
    .await;
    let r = c
        .analytics()
        .rollups()
        .create("orders", &rollup_input())
        .await
        .unwrap()
        .data;
    assert_eq!(r.id.as_deref(), Some("rol_1"));
    assert_eq!(r.attestation_status.as_deref(), Some("active"));
    assert_eq!(r.extra["endpoints"]["subscribe"], "/s");
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("POST", "/@portal/analytics/orders/rollups")
    );
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({
            "slug": "orders_hourly", "source": "orders", "dimensions": ["city"],
            "measures": [{"name": "n", "agg": "count"}, {"name": "total", "agg": "sum", "column": "amount"}],
            "windowSeconds": 3600, "reattestIntervalDays": 30, "nonPersonal": true
        })
    );

    let (server, c) = setup(
        200,
        ok(json!([{"id": "rol_1", "attestationStatus": "expiring_soon"}])),
    )
    .await;
    let l = c.analytics().rollups().list("orders").await.unwrap().data;
    assert_eq!(l[0].attestation_status.as_deref(), Some("expiring_soon"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/analytics/orders/rollups")
    );

    let (server, c) = setup(200, ok(json!({"id": "rol_1", "deleted": true}))).await;
    let d = c
        .analytics()
        .rollups()
        .delete("orders", "rol_1")
        .await
        .unwrap()
        .data;
    assert!(d.deleted && d.id == "rol_1");
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("DELETE", "/@portal/analytics/orders/rollups/rol_1")
    );

    let (server, c) = setup(
        200,
        ok(json!({"id": "rol_1", "expiresAt": "2027-01-25T10:00:00Z"})),
    )
    .await;
    let r = c
        .analytics()
        .rollups()
        .reattest("orders", "rol_1", Some(90))
        .await
        .unwrap()
        .data;
    assert_eq!(r.expires_at.as_deref(), Some("2027-01-25T10:00:00Z"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("POST", "/@portal/analytics/orders/rollups/rol_1/reattest")
    );
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body, json!({"reattestIntervalDays": 90}));
}

#[tokio::test]
async fn reattest_without_interval_sends_an_empty_object() {
    let (server, c) = setup(200, ok(json!({"id": "s1"}))).await;
    c.analytics()
        .schedules()
        .reattest("orders", "s1", None)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&only(&server).await.body).unwrap();
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn rollup_create_checks_before_sending() {
    let (server, c) = setup(201, ok(json!({}))).await;
    let ns = c.analytics();
    let r = ns.rollups();
    let mut both = rollup_input();
    both.subject_field = Some("user_id".into());
    assert!(r.create("orders", &both).await.is_err());
    let mut neither = rollup_input();
    neither.non_personal = None;
    assert!(r.create("orders", &neither).await.is_err());
    let mut false_np = rollup_input();
    false_np.non_personal = Some(false);
    assert!(r.create("orders", &false_np).await.is_err());
    let mut short = rollup_input();
    short.window_seconds = 59;
    assert!(r.create("orders", &short).await.is_err());
    let mut days = rollup_input();
    days.reattest_interval_days = Some(10);
    assert!(r.create("orders", &days).await.is_err());
    let mut none = rollup_input();
    none.measures.clear();
    assert!(r.create("orders", &none).await.is_err());
    let mut dims = rollup_input();
    dims.dimensions = vec!["d".into(); 17];
    assert!(r.create("orders", &dims).await.is_err());
    assert!(r.reattest("orders", "rol_1", Some(8)).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn per_subject_rollup_sends_subject_field() {
    let (server, c) = setup(201, ok(json!({}))).await;
    let mut i = rollup_input();
    i.non_personal = None;
    i.subject_field = Some("user_id".into());
    c.analytics().rollups().create("orders", &i).await.unwrap();
    let body: Value = serde_json::from_slice(&only(&server).await.body).unwrap();
    assert_eq!(body["subjectField"], "user_id");
    assert!(body.get("nonPersonal").is_none());
}

#[tokio::test]
async fn schedule_create_list_delete_reattest() {
    let (server, c) = setup(
        201,
        ok(json!({"id": "sch_1", "nextRunAt": "2026-10-27T11:00:00Z", "attestationStatus": "active"})),
    )
    .await;
    let s = c
        .analytics()
        .schedules()
        .create(
            "orders",
            &CreateDatasetScheduleInput {
                from_collection: "orders_raw".into(),
                interval_seconds: 3600,
                reattest_interval_days: Some(7),
            },
        )
        .await
        .unwrap()
        .data;
    assert_eq!(s.id.as_deref(), Some("sch_1"));
    assert_eq!(s.next_run_at.as_deref(), Some("2026-10-27T11:00:00Z"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("POST", "/@portal/analytics/orders/schedules")
    );
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        json!({"fromCollection": "orders_raw", "intervalSeconds": 3600, "reattestIntervalDays": 7})
    );

    let (server, c) = setup(
        200,
        ok(
            json!([{"id": "sch_1", "lastRunAt": "2026-10-27T10:00:00Z", "lastRunStatus": "ok",
                    "nextRunAt": "2026-10-27T11:00:00Z", "attestedAt": "a", "expiresAt": "b",
                    "attestationStatus": "expired"}]),
        ),
    )
    .await;
    let l = c.analytics().schedules().list("orders").await.unwrap().data;
    assert_eq!(l[0].last_run_status.as_deref(), Some("ok"));
    assert_eq!(l[0].attestation_status.as_deref(), Some("expired"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/analytics/orders/schedules")
    );

    let (server, c) = setup(200, ok(json!({"id": "sch_1", "deleted": true}))).await;
    c.analytics()
        .schedules()
        .delete("orders", "sch_1")
        .await
        .unwrap();
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("DELETE", "/@portal/analytics/orders/schedules/sch_1")
    );

    let (server, c) = setup(200, ok(json!({"id": "sch_1"}))).await;
    c.analytics()
        .schedules()
        .reattest("orders", "sch_1", Some(365))
        .await
        .unwrap();
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("POST", "/@portal/analytics/orders/schedules/sch_1/reattest")
    );
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body, json!({"reattestIntervalDays": 365}));
}

#[tokio::test]
async fn schedule_create_checks_before_sending() {
    let (server, c) = setup(201, ok(json!({}))).await;
    let s = c.analytics().schedules();
    let mk = |i: u32, d: Option<u16>| CreateDatasetScheduleInput {
        from_collection: "orders_raw".into(),
        interval_seconds: i,
        reattest_interval_days: d,
    };
    assert!(s.create("orders", &mk(59, None)).await.is_err());
    assert!(s.create("orders", &mk(60, Some(14))).await.is_err());
    assert!(s
        .create(
            "orders",
            &CreateDatasetScheduleInput {
                from_collection: "".into(),
                interval_seconds: 60,
                reattest_interval_days: None
            }
        )
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reattest_by_a_service_token_is_refused_as_human_only() {
    let (_s, c) = setup(
        403,
        json!({"code": "K9O_RBAC_DENY", "message": "only a person can re-attest",
               "context": {"reason": "human_only"}}),
    )
    .await;
    let e = c
        .analytics()
        .rollups()
        .reattest("orders", "rol_1", None)
        .await
        .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("K9O_RBAC_DENY", Some(403)));
    assert_eq!(e.context["reason"], "human_only");
    let e = c
        .analytics()
        .schedules()
        .reattest("orders", "sch_1", None)
        .await
        .unwrap_err();
    assert_eq!(e.context["reason"], "human_only");
}

#[tokio::test]
async fn top_level_rollups_list_and_read() {
    let (server, c) = setup(
        200,
        ok(json!([{"id": "rol_1", "slug": "orders_hourly", "endpoints": {"read": "r"}}])),
    )
    .await;
    let l = c.rollups().list().await.unwrap().data;
    assert_eq!(l[0].slug.as_deref(), Some("orders_hourly"));
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/rollups")
    );

    let (server, c) = setup(
        200,
        ok(json!({"cells": [{"window": "w1", "n": 4}, {"status": "suppressed", "reason": "cohort_below_k"}],
                   "coverage": {"complete": true}})),
    )
    .await;
    let r = c
        .rollups()
        .read(
            "orders_hourly",
            RollupReadOptions {
                from: Some("2026-10-26T00:00:00Z".into()),
                to: Some("2026-10-27T00:00:00Z".into()),
                dims: Some(vec!["city".into(), "channel".into()]),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.data["cells"][1]["reason"], "cohort_below_k");
    assert_eq!(r.data["coverage"]["complete"], true);
    let req = only(&server).await;
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("GET", "/@portal/rollups/orders_hourly/read")
    );
    let q: Vec<(String, String)> = req
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(q.contains(&("from".into(), "2026-10-26T00:00:00Z".into())));
    assert!(q.contains(&("to".into(), "2026-10-27T00:00:00Z".into())));
    assert!(q.contains(&("dims".into(), "city,channel".into())));
}

#[tokio::test]
async fn rollup_read_without_options_has_no_query() {
    let (server, c) = setup(200, ok(json!({"cells": []}))).await;
    c.rollups()
        .read("r", RollupReadOptions::default())
        .await
        .unwrap();
    assert!(only(&server).await.url.query().is_none());
}

#[tokio::test]
async fn rollup_read_errors_are_typed() {
    for (status, code) in [
        (400, "K9O_INVALID_FIELD_VALUE"),
        (404, "K9O_NOT_FOUND"),
        (409, "K9O_STATE_CONFLICT"),
        (503, "K9O_ANALYTICS_UNAVAILABLE"),
    ] {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(status).set_body_json(json!({"code": code, "message": "x"})),
            )
            .mount(&server)
            .await;
        let c = Client::new(ClientOpts {
            base_url: Some(server.uri()),
            project_slug: Some("portal".into()),
            retry_config: Some(k9o::RetryConfig {
                max_retries: 0,
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let e = c
            .rollups()
            .read("r", RollupReadOptions::default())
            .await
            .unwrap_err();
        assert_eq!((e.code.as_str(), e.status), (code, Some(status)));
    }
}

#[tokio::test]
async fn needs_a_project() {
    let c = Client::new(ClientOpts {
        base_url: Some("http://127.0.0.1:9".into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        c.analytics().list().await.unwrap_err().code,
        "K9O_MISSING_PROJECT_CONTEXT"
    );
    assert_eq!(
        c.rollups().list().await.unwrap_err().code,
        "K9O_MISSING_PROJECT_CONTEXT"
    );
    assert_eq!(
        c.analytics()
            .rollups()
            .list("orders")
            .await
            .unwrap_err()
            .code,
        "K9O_MISSING_PROJECT_CONTEXT"
    );
}

#[tokio::test]
async fn lists_read_as_an_array_or_as_an_object_holding_one_array() {
    let (_s, c) = setup(
        200,
        ok(json!({"rollups": [{"id": "rollup_1", "slug": "a", "datasetSlug": "orders", "enabled": true}]})),
    )
    .await;
    let l = c.rollups().list().await.unwrap().data;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].slug.as_deref(), Some("a"));
    assert_eq!(l[0].extra["datasetSlug"], "orders");
    let ids: Vec<_> = l.iter().filter_map(|r| r.id.clone()).collect();
    assert_eq!(ids, vec!["rollup_1"]);

    let (_s, c) = setup(200, ok(json!({"schedules": [{"id": "s1"}, {"id": "s2"}]}))).await;
    let l = c.analytics().schedules().list("orders").await.unwrap().data;
    assert_eq!(l.len(), 2);

    let (_s, c) = setup(200, ok(json!({"rollups": []}))).await;
    assert!(c
        .analytics()
        .rollups()
        .list("orders")
        .await
        .unwrap()
        .data
        .is_empty());
}
