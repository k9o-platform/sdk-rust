//! Vector collections: method, path and body of `nearest` and `upsert`, decoding of the gateway's
//! recorded answers (tests/fixtures), client-side validation that sends nothing, and gateway errors.

use k9o::{Client, ClientOpts, KError, NearestOptions, VectorRow, VectorSource};
use serde_json::{json, Map, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

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

async fn sent(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

async fn last(server: &MockServer) -> (String, String, Value) {
    let reqs = server.received_requests().await.unwrap();
    let r = reqs.last().unwrap();
    (
        r.method.to_string(),
        r.url.path().to_string(),
        serde_json::from_slice(&r.body).unwrap(),
    )
}

fn assert_invalid(err: KError) {
    assert_eq!(err.code, "K9O_INVALID_FIELD_VALUE", "{err}");
}

fn row<'a>(vector: &'a [f64]) -> VectorRow<'a> {
    VectorRow {
        id: None,
        vector,
        source: VectorSource {
            handle: "probe",
            chunk_index: 0,
        },
        content_hash: None,
        text: None,
        metadata: None,
        subject_ids: None,
    }
}

#[tokio::test]
async fn nearest_posts_the_body_and_decodes_the_recorded_answer() {
    let (server, c) = setup(200, fixture("vectors_nearest_200.json"), Some("portal")).await;
    let mut filters = Map::new();
    filters.insert("metadata.lang".into(), json!("en"));
    filters.insert("chunk".into(), json!(1));
    let r = c
        .vectors()
        .nearest(
            "_vec_example",
            &[1.0, 0.0, 0.0],
            NearestOptions {
                top_k: Some(2),
                min_score: Some(0.4),
                metric: Some("euclidean"),
                filters: Some(&filters),
            },
        )
        .await
        .unwrap();
    let (m, p, body) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("POST", "/@portal/vectors/_vec_example/search")
    );
    assert_eq!(
        body,
        json!({
            "queryVector": [1.0, 0.0, 0.0],
            "topK": 2,
            "minScore": 0.4,
            "metric": "euclidean",
            "filters": { "metadata.lang": "en", "chunk": 1 }
        })
    );
    assert_eq!(r.data.collection, "_vec_example");
    let ids: Vec<&str> = r.data.results.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(ids, ["p1", "p2"]);
    let (a, b) = (&r.data.results[0], &r.data.results[1]);
    assert_eq!(
        (a.score, a.text.as_deref(), a.chunk),
        (1.0, Some("x axis"), Some(0))
    );
    assert_eq!((a.content_hash.clone(), a.model.clone()), (None, None));
    assert_eq!(a.source["handle"], "probe");
    assert_eq!(a.metadata["lang"], "en");
    assert_eq!(
        (
            b.score,
            b.text.clone(),
            b.content_hash.as_deref(),
            b.model.as_deref()
        ),
        (0.8, None, Some("h2"), Some("m1"))
    );
    assert_eq!(b.source["extra"], true);
}

#[tokio::test]
async fn nearest_sends_only_the_vector_without_options() {
    let (server, c) = setup(200, fixture("vectors_nearest_200.json"), Some("portal")).await;
    c.vectors()
        .nearest("_n", &[1.0], NearestOptions::default())
        .await
        .unwrap();
    assert_eq!(last(&server).await.2, json!({ "queryVector": [1.0] }));
}

#[tokio::test]
async fn nearest_accepts_the_bounds() {
    let (server, c) = setup(200, fixture("vectors_nearest_200.json"), Some("portal")).await;
    let long = vec![0.5; 16000];
    let v = c.vectors();
    v.nearest(
        "_n",
        &long,
        NearestOptions {
            top_k: Some(1000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    v.nearest(
        "_n",
        &[1.0],
        NearestOptions {
            top_k: Some(1),
            min_score: Some(-1.0),
            metric: Some("dot"),
            filters: None,
        },
    )
    .await
    .unwrap();
    v.nearest(
        "_n",
        &[1.0],
        NearestOptions {
            metric: Some("cosine"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(sent(&server).await, 3);
}

#[tokio::test]
async fn nearest_refuses_bad_input_before_sending() {
    let (server, c) = setup(200, fixture("vectors_nearest_200.json"), Some("portal")).await;
    let v = c.vectors();
    let too_long = vec![1.0; 16001];
    let mut bad_filter = Map::new();
    bad_filter.insert("id".into(), json!(["a"]));
    let o = NearestOptions::default;
    assert_invalid(v.nearest("notes", &[1.0], o()).await.unwrap_err());
    assert_invalid(v.nearest("", &[1.0], o()).await.unwrap_err());
    assert_invalid(v.nearest("_n", &[], o()).await.unwrap_err());
    assert_invalid(v.nearest("_n", &too_long, o()).await.unwrap_err());
    assert_invalid(v.nearest("_n", &[1.0, f64::NAN], o()).await.unwrap_err());
    assert_invalid(v.nearest("_n", &[f64::INFINITY], o()).await.unwrap_err());
    assert_invalid(
        v.nearest(
            "_n",
            &[1.0],
            NearestOptions {
                top_k: Some(0),
                ..o()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        v.nearest(
            "_n",
            &[1.0],
            NearestOptions {
                top_k: Some(1001),
                ..o()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        v.nearest(
            "_n",
            &[1.0],
            NearestOptions {
                metric: Some("l2"),
                ..o()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        v.nearest(
            "_n",
            &[1.0],
            NearestOptions {
                min_score: Some(f64::NAN),
                ..o()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        v.nearest(
            "_n",
            &[1.0],
            NearestOptions {
                filters: Some(&bad_filter),
                ..o()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(sent(&server).await, 0);
}

#[tokio::test]
async fn vectors_need_a_bound_project() {
    let (server, c) = setup(200, fixture("vectors_nearest_200.json"), None).await;
    let e = c
        .vectors()
        .nearest("_n", &[1.0], NearestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    let e = c.vectors().upsert("_n", &[row(&[1.0])]).await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    assert_eq!(sent(&server).await, 0);
}

#[tokio::test]
async fn nearest_decodes_a_404_for_a_collection_never_upserted() {
    let (_s, c) = setup(
        404,
        fixture("vectors_collection_not_found_404.json"),
        Some("portal"),
    )
    .await;
    let e = c
        .vectors()
        .nearest("_vec_example", &[1.0, 0.0, 0.0], NearestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_COLLECTION_NOT_FOUND");
    assert_eq!(e.status, Some(404));
    assert!(e.message.contains("_vec_example"));
}

#[tokio::test]
async fn nearest_decodes_a_dimension_mismatch() {
    let (_s, c) = setup(
        400,
        fixture("vectors_invalid_dimension_400.json"),
        Some("portal"),
    )
    .await;
    let e = c
        .vectors()
        .nearest("_vec_example", &[1.0, 0.0], NearestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_VECTOR_DIMENSION");
    assert_eq!(e.status, Some(400));
    assert_eq!(e.context["collection"], "_vec_example");
    assert_eq!(e.context["queryDim"], 2);
}

#[tokio::test]
async fn upsert_posts_rows_and_decodes_the_count() {
    let (server, c) = setup(200, fixture("vectors_upsert_200.json"), Some("portal")).await;
    let mut meta = Map::new();
    meta.insert("lang".into(), json!("en"));
    let subjects = ["eu_YmGE3Z6nkv5i"];
    let v = [0.0, 0.0, 1.0];
    let full = VectorRow {
        id: Some("rust-1"),
        vector: &v,
        source: VectorSource {
            handle: "intro",
            chunk_index: 2,
        },
        content_hash: Some("h"),
        text: Some("z axis"),
        metadata: Some(&meta),
        subject_ids: Some(&subjects),
    };
    let plain = row(&[1.0, 0.0, 0.0]);
    let r = c
        .vectors()
        .upsert("_vec_example", &[full, plain])
        .await
        .unwrap();
    assert_eq!(
        (r.data.upserted, r.data.collection.as_str()),
        (3, "_vec_example")
    );
    let (m, p, body) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("POST", "/@portal/vectors/_vec_example/upsert")
    );
    assert_eq!(
        body,
        json!({ "rows": [
            { "id": "rust-1", "vector": [0.0, 0.0, 1.0], "_source": { "handle": "intro", "chunkIndex": 2 },
              "contentHash": "h", "text": "z axis", "metadata": { "lang": "en" }, "subjectIds": ["eu_YmGE3Z6nkv5i"] },
            { "vector": [1.0, 0.0, 0.0], "_source": { "handle": "probe", "chunkIndex": 0 } }
        ] })
    );
}

#[tokio::test]
async fn upsert_refuses_bad_input_before_sending() {
    let (server, c) = setup(200, fixture("vectors_upsert_200.json"), Some("portal")).await;
    let v = c.vectors();
    let good = [1.0];
    assert_invalid(v.upsert("notes", &[row(&good)]).await.unwrap_err());
    assert_invalid(v.upsert("_n", &[]).await.unwrap_err());
    let many: Vec<VectorRow> = (0..1001).map(|_| row(&good)).collect();
    assert_invalid(v.upsert("_n", &many).await.unwrap_err());
    assert_invalid(v.upsert("_n", &[row(&[])]).await.unwrap_err());
    assert_invalid(
        v.upsert("_n", &[row(&good), row(&[f64::NAN])])
            .await
            .unwrap_err(),
    );
    let long = vec![0.5; 16001];
    assert_invalid(v.upsert("_n", &[row(&long)]).await.unwrap_err());
    assert_eq!(sent(&server).await, 0);
    let ok: Vec<VectorRow> = (0..1000).map(|_| row(&good)).collect();
    v.upsert("_n", &ok).await.unwrap();
    assert_eq!(sent(&server).await, 1);
}
