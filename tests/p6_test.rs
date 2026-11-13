//! P6 surface-family tests (studio / compute.llms / realtime / org
//! default-LLM). Routes verified against a wiremock gateway.

use k9o::{realtime_topic, Client, ClientOpts, LlmInvokeInput, LlmInvokeParams};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> Client {
    Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn workflows_crud_and_runs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/@portal/workflows"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[],"meta":{}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/@portal/workflows/wfl_x/runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"id":"wfr_1","workflowId":"wfl_x","projectId":"p","status":"pending"},
            "meta": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/@portal/workflows/wfl_x/runs/wfr_1/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"id":"wfr_1","workflowId":"wfl_x","projectId":"p","status":"cancelled"},
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = client(&server);

    c.studio()
        .workflows()
        .list(Some("active"), None, None)
        .await
        .unwrap();
    let run = c
        .studio()
        .workflows()
        .trigger("wfl_x", &serde_json::json!({"foo":"bar"}))
        .await
        .unwrap();
    assert_eq!(run.data.status, "pending");
    let cancelled = c
        .studio()
        .workflows()
        .runs()
        .cancel("wfl_x", "wfr_1")
        .await
        .unwrap();
    assert_eq!(cancelled.data.status, "cancelled");
}

#[tokio::test]
async fn dashboards_pin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@portal/dashboards/dsh_x/pin"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"id":"dsh_x","projectId":"p","name":"ops","status":"active","pinnedToWorkspace":true},
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = client(&server);

    let pinned = c.studio().dashboards().pin("dsh_x").await.unwrap();
    assert!(pinned.data.pinned_to_workspace);
}

#[tokio::test]
async fn llms_invoke() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/@portal/compute/llms/mistral-7b/invoke"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"modelId":"mistral-7b","content":"hi","finishReason":"stop","usage":{"promptTokens":1,"completionTokens":1,"totalTokens":2},"usageType":"sdk.direct"},
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = client(&server);
    let resp = c
        .compute()
        .llms()
        .invoke(
            "mistral-7b",
            LlmInvokeInput {
                prompt: "hello",
                params: LlmInvokeParams::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(resp.data.content, "hi");
    assert_eq!(resp.data.usage.total_tokens, 2);
}

#[tokio::test]
async fn llms_invoke_stream_yields_chunks() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/@portal/compute/llms/gpt-4o/invoke-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    let mut stream = c
        .compute()
        .llms()
        .invoke_stream(
            "gpt-4o",
            LlmInvokeInput {
                prompt: "hi",
                params: LlmInvokeParams::default(),
            },
        )
        .await
        .unwrap();
    let mut acc = String::new();
    let mut last_finish = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if let Some(choice) = chunk.choices.first() {
            if let Some(text) = &choice.delta.content {
                acc.push_str(text);
            }
            last_finish = choice.finish_reason.clone();
        }
    }
    assert_eq!(acc, "Hello");
    assert_eq!(last_finish.as_deref(), Some("stop"));
}

#[tokio::test]
async fn org_default_llm_get_set() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/default-llm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data":{"orgId":"org_x","defaultModelId":"mistral-7b","updatedAt":"t","updatedBy":null},
            "meta":{}
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/default-llm"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data":{"orgId":"org_x","defaultModelId":"claude-3-5-sonnet"},
            "meta":{}
        })))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        ..Default::default()
    })
    .unwrap();
    let got = c.org().default_llm_get().await.unwrap();
    assert_eq!(got.data.default_model_id, "mistral-7b");
    c.org().default_llm_set("claude-3-5-sonnet").await.unwrap();
}

#[tokio::test]
async fn realtime_topic_builders() {
    assert_eq!(realtime_topic::collection("orders"), "collections.orders.*");
    assert_eq!(realtime_topic::workflow_runs(), "studio.workflows.runs.*");
    assert_eq!(
        realtime_topic::dashboard_refresh("dsh_x"),
        "studio.dashboards.refresh.dsh_x"
    );
    assert_eq!(
        realtime_topic::function_invocations("fn_x"),
        "forge.functions.invocations.fn_x"
    );
    assert_eq!(
        realtime_topic::audit_org("role_granted"),
        "audit.org.role_granted"
    );
}

#[tokio::test]
async fn realtime_subscribe_sse() {
    let server = MockServer::start().await;
    let sse = "data: {\"topic\":\"collections.orders.*\",\"eventType\":\"row_inserted\",\"payload\":{},\"occurredAt\":\"t1\"}\n\ndata: {\"topic\":\"collections.orders.*\",\"eventType\":\"row_updated\",\"payload\":{},\"occurredAt\":\"t2\"}\n\n";
    Mock::given(method("GET"))
        .and(path("/@portal/realtime/subscribe"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let c = client(&server);
    let mut sub = c
        .realtime()
        .subscribe(&realtime_topic::collection("orders"), None)
        .await
        .unwrap();
    let mut types = Vec::new();
    while types.len() < 2 {
        types.push(sub.next().await.unwrap().unwrap().event_type);
    }
    sub.close();
    assert!(sub.next().await.is_none());
    assert_eq!(types, vec!["row_inserted", "row_updated"]);
}
