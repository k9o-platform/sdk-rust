//! `client.query` and the search surface: wire shape, the shared translation vectors and validation.

use k9o::{
    translate_named_params, Client, ClientOpts, Params, SearchInput, TextSearchOptions,
    TextSearchType, VectorMetric, VectorSearchOptions,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap()
}

async fn mount(server: &MockServer, body: Value) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn named(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn shared_vectors() {
    let cases: Vec<(&str, Value, &str, Vec<Value>)> = vec![
        (
            "SELECT * FROM users WHERE data->>'role' = :role",
            json!({"role": "admin"}),
            "SELECT * FROM users WHERE data->>'role' = $1",
            vec![json!("admin")],
        ),
        (
            "SELECT :a, :b, :a",
            json!({"a": 1, "b": 2}),
            "SELECT $1, $2, $1",
            vec![json!(1), json!(2)],
        ),
        (
            "SELECT ':not_a_param', \":also_not\", x::int FROM t WHERE y = :y",
            json!({"y": true}),
            "SELECT ':not_a_param', \":also_not\", x::int FROM t WHERE y = $1",
            vec![json!(true)],
        ),
        (
            "SELECT 'it''s :no' , :v -- :comment\nFROM t",
            json!({"v": null}),
            "SELECT 'it''s :no' , $1 -- :comment\nFROM t",
            vec![Value::Null],
        ),
        (
            "SELECT /* :c /* :d */ :e */ :f",
            json!({"f": 1}),
            "SELECT /* :c /* :d */ :e */ $1",
            vec![json!(1)],
        ),
        (
            "SELECT $$ :x $$, $q$ :y $q$, :z",
            json!({"z": "s"}),
            "SELECT $$ :x $$, $q$ :y $q$, $1",
            vec![json!("s")],
        ),
        (
            "SELECT arr[1:2], a:b, :n",
            json!({"n": 5}),
            "SELECT arr[1:2], a:b, $1",
            vec![json!(5)],
        ),
        (
            "SELECT E'it\\'s :no', :v",
            json!({"v": 1}),
            "SELECT E'it\\'s :no', $1",
            vec![json!(1)],
        ),
        (
            "SELECT \"a\"\":b\", :v, 'é :x', :w",
            json!({"v": 1, "w": 2, "extra": 3}),
            "SELECT \"a\"\":b\", $1, 'é :x', $2",
            vec![json!(1), json!(2)],
        ),
    ];
    for (sql, map, want_sql, want_params) in cases {
        let (got, params) = translate_named_params(sql, &named(map)).unwrap();
        assert_eq!(got, want_sql);
        assert_eq!(params, want_params);
    }
}

#[test]
fn vector_8_missing_and_vector_9_object() {
    let e = translate_named_params("SELECT :missing", &Map::new()).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert!(e.message.contains(":missing"));
    let e = translate_named_params("SELECT :a", &named(json!({"a": {}}))).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    let e = translate_named_params("SELECT :a", &named(json!({"a": [1]}))).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
}

#[tokio::test]
async fn query_wire_shapes() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({"data": [{"id": "a"}], "meta": {"rawQuery": {"intent": "select", "rowCount": 1}}}),
    )
    .await;
    let c = client(&server);

    // vector 10: list params unchanged
    let r = c
        .query::<Value>("SELECT $1", vec![json!("x")])
        .await
        .unwrap();
    assert_eq!(r.data[0]["id"], "a");
    let rq = r.meta.raw_query.unwrap();
    assert_eq!((rq.intent.as_str(), rq.row_count), ("select", 1));
    // vector 11: no params, no key
    c.query::<Value>("SELECT 1", Params::None).await.unwrap();
    c.query::<Value>("SELECT :a, :b, :a", named(json!({"a": 1, "b": "two"})))
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 3);
    assert!(reqs
        .iter()
        .all(|r| r.method.as_str() == "POST" && r.url.path() == "/@portal/query"));
    let bodies: Vec<Value> = reqs
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0], json!({"sql": "SELECT $1", "params": ["x"]}));
    assert_eq!(bodies[1], json!({"sql": "SELECT 1"}));
    assert!(bodies[1].get("params").is_none());
    assert_eq!(
        bodies[2],
        json!({"sql": "SELECT $1, $2, $1", "params": [1, "two"]})
    );
}

#[tokio::test]
async fn query_validation_sends_nothing() {
    let server = MockServer::start().await;
    mount(&server, json!({"data": [], "meta": {}})).await;
    let c = client(&server);
    let e = c
        .query::<Value>("SELECT :missing", Map::new())
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    let e = c
        .query::<Value>("SELECT $1", vec![json!({"a": 1})])
        .await
        .unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn query_of(req: &wiremock::Request) -> BTreeMap<String, String> {
    req.url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn search_wire_shapes() {
    let server = MockServer::start().await;
    mount(&server, json!({"data": [], "meta": {}})).await;
    let c = client(&server);

    c.search::<Value>(
        "posts",
        SearchInput {
            column: "body",
            q: "red shoes",
            language: Some("french"),
            search_type: Some(TextSearchType::Websearch),
            limit: Some(3),
            cursor: Some("cur_1"),
        },
    )
    .await
    .unwrap();
    c.search::<Value>(
        "posts",
        SearchInput {
            column: "body",
            q: "x",
            language: Some("english"),
            search_type: Some(TextSearchType::Plain),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    c.from("posts")
        .select()
        .vector_search(
            "embedding",
            &[0.5, -1.0, 2.0],
            VectorSearchOptions {
                metric: VectorMetric::Dot,
                limit: Some(7),
            },
        )
        .execute::<Value>()
        .await
        .unwrap();
    c.from("posts")
        .select()
        .vector_search("embedding", &[1.0], VectorSearchOptions::default())
        .execute::<Value>()
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 4);
    assert!(reqs[..2]
        .iter()
        .all(|r| r.method.as_str() == "GET" && r.url.path() == "/@portal/data/posts"));
    assert!(reqs[2..]
        .iter()
        .all(|r| r.method.as_str() == "POST" && r.url.path() == "/@portal/data/posts/search"));
    assert_eq!(
        query_of(&reqs[0]),
        map(&[
            ("textSearch", "body:red shoes"),
            ("textSearchLanguage", "french"),
            ("textSearchType", "websearch"),
            ("limit", "3"),
            ("cursor", "cur_1"),
        ])
    );
    assert_eq!(query_of(&reqs[1]), map(&[("textSearch", "body:x")]));
    let body = |i: usize| serde_json::from_slice::<Value>(&reqs[i].body).unwrap();
    assert_eq!(
        body(2),
        json!({"vectorSearch": {"column": "embedding", "vector": [0.5, -1.0, 2.0], "metric": "dot", "limit": 7}})
    );
    assert_eq!(
        body(3),
        json!({"vectorSearch": {"column": "embedding", "vector": [1.0]}})
    );
}

#[tokio::test]
async fn search_validation_sends_nothing() {
    let server = MockServer::start().await;
    mount(&server, json!({"data": [], "meta": {}})).await;
    let c = client(&server);
    let opts = TextSearchOptions::default;
    let vopts = VectorSearchOptions::default;
    let results = vec![
        c.from("posts")
            .select()
            .text_search("bad col", "x", opts())
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .text_search("body", "", opts())
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .text_search(
                "body",
                "x",
                TextSearchOptions {
                    language: Some("en glish"),
                    ..opts()
                },
            )
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .vector_search("1bad", &[1.0], vopts())
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .vector_search("e", &[], vopts())
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .vector_search("e", &[f64::NAN], vopts())
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .vector_search(
                "e",
                &[1.0],
                VectorSearchOptions {
                    limit: Some(1001),
                    ..vopts()
                },
            )
            .execute::<Value>()
            .await,
        c.from("posts")
            .select()
            .vector_search(
                "e",
                &[1.0],
                VectorSearchOptions {
                    limit: Some(0),
                    ..vopts()
                },
            )
            .execute::<Value>()
            .await,
        c.search::<Value>(
            "posts",
            SearchInput {
                q: "x",
                ..Default::default()
            },
        )
        .await,
    ];
    for (i, r) in results.into_iter().enumerate() {
        let e = r.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE", "case {i}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
