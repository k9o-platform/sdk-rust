//! Writes that wait for approval, function triggers, schedules and end-user role grants:
//! method, path, query and body of every call, decoding of the gateway's recorded answers
//! (tests/fixtures) through the SDK's real types, 204 handling and client-side validation.

use k9o::{
    Client, ClientOpts, CreateScheduleInput, CreateTriggerInput, GrantEndUserRoleInput, KError,
    ListEndUsersOptions, ListRoleGrantsOptions, ListScheduleRunsOptions, ListSchedulesOptions,
    UpdateScheduleInput, UpdateTriggerInput, WriteOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn setup(status: u16, body: Option<Value>) -> (MockServer, Client) {
    let server = MockServer::start().await;
    let mut t = ResponseTemplate::new(status);
    if let Some(b) = body {
        t = t.set_body_json(b);
    }
    Mock::given(any()).respond_with(t).mount(&server).await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();
    (server, c)
}

async fn last(server: &MockServer) -> (String, String, String, Value) {
    let reqs = server.received_requests().await.unwrap();
    let r = reqs.last().unwrap();
    let body = if r.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&r.body).unwrap()
    };
    (
        r.method.to_string(),
        r.url.path().to_string(),
        r.url.query().unwrap_or("").to_string(),
        body,
    )
}

fn call(m: &str, p: &str, q: &str, b: Value) -> (String, String, String, Value) {
    (m.into(), p.into(), q.into(), b)
}

async fn sent(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn assert_invalid(err: KError) {
    assert_eq!(err.code, "K9O_INVALID_FIELD_VALUE", "{err}");
}

#[derive(Debug, Deserialize, Serialize)]
struct Row {
    id: String,
    #[serde(default)]
    val: Option<String>,
}

fn applied_row() -> Value {
    json!({"data": {"id": "k1", "val": "v"}, "meta": {}})
}

// --- A. requireApproval -------------------------------------------------------------------

#[tokio::test]
async fn queued_insert_fixture_is_recognised_as_queued() {
    let (server, c) = setup(202, Some(fixture("write_queued_insert_202.json"))).await;
    let out = c
        .from("conformance_kv")
        .insert_builder(&json!({"id": "contract2-probe-0001", "val": "contract2-probe"}))
        .require_approval()
        .execute::<Row>()
        .await
        .unwrap();
    assert!(out.is_queued());
    let WriteOutcome::Queued(q) = out else {
        panic!("expected Queued")
    };
    assert_eq!(q.data.id, "apr_5oLshD2MVT27");
    assert_eq!(q.data.status, "pending_approval");
    assert_eq!(q.data.operation, "insert");
    assert_eq!(q.data.maker_id, "usr_conformance0001");
    assert_eq!(q.data.target_record_id, None);
    assert_eq!(q.data.proposed_payload["id"], "contract2-probe-0001");
    assert_eq!(q.meta.approval.unwrap().status, "pending_approval");
    assert_eq!(
        last(&server).await,
        call(
            "POST",
            "/@portal/data/conformance_kv",
            "requireApproval=true",
            json!({"id": "contract2-probe-0001", "val": "contract2-probe"})
        )
    );
}

#[tokio::test]
async fn applied_write_with_the_flag_is_not_queued() {
    // An auto_approve policy applies the write even with the flag: 201 and the row.
    let (_s, c) = setup(201, Some(applied_row())).await;
    let out = c
        .from("kv")
        .insert_builder(&json!({"id": "k1", "val": "v"}))
        .require_approval()
        .execute::<Row>()
        .await
        .unwrap();
    assert!(!out.is_queued());
    assert!(out.queued().is_none());
    let WriteOutcome::Applied(r) = out else {
        panic!("expected Applied")
    };
    assert_eq!(r.data.id, "k1");
    assert_eq!(r.data.val.as_deref(), Some("v"));
}

#[tokio::test]
async fn a_row_whose_status_column_reads_pending_approval_is_still_applied() {
    // Without meta.approval the data is a row, whatever its columns say.
    let body = json!({"data": {"id": "k1", "status": "pending_approval"}, "meta": {}});
    let (_s, c) = setup(200, Some(body)).await;
    let out = c
        .from("kv")
        .update(json!({"status": "pending_approval"}))
        .eq("id", "k1")
        .require_approval()
        .execute::<Value>()
        .await
        .unwrap();
    assert!(!out.is_queued());
}

#[tokio::test]
async fn queued_update_and_delete_wire() {
    let queued = |op: &str, target: Value, payload: Value| {
        json!({"data": {"id": "apr_2", "status": "pending_approval", "collection": "orders",
                        "operation": op, "makerId": "usr_1", "proposedPayload": payload,
                        "targetRecordId": target},
               "meta": {"approval": {"status": "pending_approval"}}})
    };

    let (server, c) = setup(
        202,
        Some(queued("update", json!("o 1"), json!({"total": 5}))),
    )
    .await;
    let out = c
        .from("orders")
        .update(json!({"total": 5}))
        .eq("id", "o 1")
        .require_approval()
        .execute::<Value>()
        .await
        .unwrap();
    assert_eq!(
        out.queued().unwrap().target_record_id.as_deref(),
        Some("o 1")
    );
    assert_eq!(
        last(&server).await,
        call(
            "PATCH",
            "/@portal/data/orders/o%201",
            "requireApproval=true",
            json!({"total": 5})
        )
    );

    let (server, c) = setup(202, Some(queued("delete", json!("o1"), json!({})))).await;
    let out = c
        .from("orders")
        .delete()
        .eq("id", "o1")
        .require_approval()
        .execute::<Value>()
        .await
        .unwrap();
    assert_eq!(out.queued().unwrap().operation, "delete");
    assert_eq!(
        last(&server).await,
        call(
            "DELETE",
            "/@portal/data/orders/o1",
            "requireApproval=true",
            Value::Null
        )
    );
}

#[tokio::test]
async fn insert_builder_without_the_flag_sends_no_query() {
    let (server, c) = setup(201, Some(applied_row())).await;
    let r = c
        .from("kv")
        .insert_builder(&json!({"id": "k1"}))
        .execute::<Row>()
        .await
        .unwrap();
    assert_eq!(r.data.id, "k1");
    assert_eq!(
        last(&server).await,
        call("POST", "/@portal/data/kv", "", json!({"id": "k1"}))
    );
}

#[tokio::test]
async fn require_approval_validation_sends_nothing() {
    let (server, c) = setup(202, None).await;
    // update/delete without .eq("id", ..)
    assert_invalid(
        c.from("kv")
            .update(json!({"a": 1}))
            .require_approval()
            .execute::<Value>()
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.from("kv")
            .delete()
            .require_approval()
            .execute::<Value>()
            .await
            .unwrap_err(),
    );
    // bad collection name
    assert_invalid(
        c.from("Bad Name")
            .insert_builder(&json!({}))
            .require_approval()
            .execute::<Value>()
            .await
            .unwrap_err(),
    );
    assert_eq!(sent(&server).await, 0);
}

#[tokio::test]
async fn existing_insert_is_unchanged() {
    let (server, c) = setup(201, Some(applied_row())).await;
    let r = c
        .from("kv")
        .insert::<Row, _>(&json!({"id": "k1"}))
        .await
        .unwrap();
    assert_eq!(r.data.id, "k1");
    assert_eq!(
        last(&server).await,
        call("POST", "/@portal/data/kv", "", json!({"id": "k1"}))
    );
}

// --- B. function triggers -----------------------------------------------------------------

fn trigger() -> Value {
    json!({"id": "0b8f6a52-3f0e-4c8e-9a47-5d2b0f1c7e11", "functionName": "notify",
           "collection": "orders", "events": ["insert", "update"], "filter": null,
           "enabled": true, "logOkInvocations": false, "createdAt": "2026-10-26T17:59:56.815Z"})
}

#[tokio::test]
async fn triggers_list_empty_fixture() {
    let (server, c) = setup(200, Some(fixture("function_triggers_list_empty.json"))).await;
    let r = c.functions().triggers().list("w4c-probe").await.unwrap();
    assert!(r.data.is_empty());
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            "/@portal/functions/w4c-probe/triggers",
            "",
            Value::Null
        )
    );
}

#[tokio::test]
async fn triggers_create_update_delete_wire() {
    let (server, c) = setup(201, Some(json!({"data": trigger(), "meta": {}}))).await;
    let filter = json!({"eq": ["resource.status", "paid"]});
    let t = c
        .functions()
        .triggers()
        .create(
            "notify",
            CreateTriggerInput {
                collection: "orders",
                events: &["insert", "update"],
                filter: Some(&filter),
                enabled: Some(true),
                log_ok_invocations: Some(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(t.data.id, "0b8f6a52-3f0e-4c8e-9a47-5d2b0f1c7e11");
    assert_eq!(t.data.events, vec!["insert", "update"]);
    assert_eq!(t.data.filter, None);
    assert_eq!(t.data.collection.as_deref(), Some("orders"));
    assert_eq!(
        last(&server).await,
        call(
            "POST",
            "/@portal/functions/notify/triggers",
            "",
            json!({"collection": "orders", "events": ["insert", "update"], "filter": filter,
                   "enabled": true, "logOkInvocations": false})
        )
    );

    // Only the required fields: nothing else is sent.
    c.functions()
        .triggers()
        .create(
            "notify",
            CreateTriggerInput {
                collection: "orders",
                events: &["delete"],
                filter: None,
                enabled: None,
                log_ok_invocations: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last(&server).await.3,
        json!({"collection": "orders", "events": ["delete"]})
    );

    let id = "0b8f6a52-3f0e-4c8e-9a47-5d2b0f1c7e11";
    c.functions()
        .triggers()
        .update(
            "notify",
            id,
            UpdateTriggerInput {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last(&server).await,
        call(
            "PATCH",
            &format!("/@portal/functions/notify/triggers/{id}"),
            "",
            json!({"enabled": false})
        )
    );

    // filter: Some(None) clears it (explicit null); events replace the list.
    c.functions()
        .triggers()
        .update(
            "notify",
            id,
            UpdateTriggerInput {
                events: Some(&["upsert"]),
                filter: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last(&server).await.3,
        json!({"events": ["upsert"], "filter": null})
    );
}

#[tokio::test]
async fn triggers_delete_answers_204_without_a_body() {
    let (server, c) = setup(204, None).await;
    let id = "0b8f6a52-3f0e-4c8e-9a47-5d2b0f1c7e11";
    let r = c.functions().triggers().delete("notify", id).await.unwrap();
    assert_eq!(r.data, ());
    assert_eq!(
        last(&server).await,
        call(
            "DELETE",
            &format!("/@portal/functions/notify/triggers/{id}"),
            "",
            Value::Null
        )
    );
}

#[tokio::test]
async fn triggers_validation_sends_nothing() {
    let (server, c) = setup(200, Some(json!({"data": [], "meta": {}}))).await;
    let t = c.functions().triggers();
    let mk = |collection: &'static str, events: &'static [&'static str]| CreateTriggerInput {
        collection,
        events,
        filter: None,
        enabled: None,
        log_ok_invocations: None,
    };
    assert_invalid(t.create("f", mk("orders", &[])).await.unwrap_err());
    assert_invalid(
        t.create(
            "f",
            mk(
                "orders",
                &["insert", "update", "delete", "upsert", "insert"],
            ),
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        t.create("f", mk("orders", &["truncate"]))
            .await
            .unwrap_err(),
    );
    assert_invalid(t.create("f", mk("1orders", &["insert"])).await.unwrap_err());
    assert_invalid(t.create("f", mk("or-ders", &["insert"])).await.unwrap_err());
    assert_invalid(t.create("", mk("orders", &["insert"])).await.unwrap_err());
    assert_invalid(
        t.update("f", "id", UpdateTriggerInput::default())
            .await
            .unwrap_err(),
    );
    assert_invalid(
        t.update(
            "f",
            "id",
            UpdateTriggerInput {
                events: Some(&[]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(t.delete("f", "").await.unwrap_err());
    assert_eq!(sent(&server).await, 0);
}

// --- B.7 schedules ------------------------------------------------------------------------

fn schedule() -> Value {
    json!({"id": "11111111-2222-3333-4444-555555555555", "functionName": "nightly-report",
           "scheduleExpression": "0 0 * * *", "scheduleTimezone": "Europe/Berlin",
           "catchupPolicy": "skip", "collection": null, "enabled": true, "status": "active",
           "logOkInvocations": false, "createdAt": "2026-10-26T17:59:56.815Z",
           "lastInvocationAt": null, "nextInvocationAt": null})
}

#[tokio::test]
async fn schedules_list_empty_fixture_and_query() {
    let (server, c) = setup(200, Some(fixture("schedules_list_empty.json"))).await;
    let r = c
        .schedules()
        .list(ListSchedulesOptions {
            status: Some("paused"),
            function_name: Some("nightly-report"),
            limit: Some(2),
            cursor: Some("abc"),
        })
        .await
        .unwrap();
    assert!(r.data.is_empty());
    assert_eq!(r.meta.cursor.unwrap().next, None);
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            "/@portal/forge/schedules",
            "status=paused&function_name=nightly-report&limit=2&cursor=abc",
            Value::Null
        )
    );

    c.schedules()
        .list(ListSchedulesOptions::default())
        .await
        .unwrap();
    assert_eq!(last(&server).await.2, "");
}

#[tokio::test]
async fn schedules_get_create_update_wire() {
    let (server, c) = setup(200, Some(json!({"data": schedule(), "meta": {}}))).await;
    let id = "11111111-2222-3333-4444-555555555555";

    let s = c.schedules().get(id).await.unwrap();
    assert_eq!(s.data.schedule_expression, "0 0 * * *");
    assert_eq!(s.data.status, "active");
    assert_eq!(s.data.collection, None);
    assert_eq!(s.data.next_invocation_at, None);
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            &format!("/@portal/forge/schedules/{id}"),
            "",
            Value::Null
        )
    );

    c.schedules()
        .create(CreateScheduleInput {
            function_name: "nightly-report",
            schedule_expression: "@daily",
            schedule_timezone: Some("Europe/Berlin"),
            catchup_policy: Some("run_once"),
            collection: Some("orders"),
            enabled: Some(true),
            log_ok_invocations: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(
        last(&server).await,
        call(
            "POST",
            "/@portal/forge/schedules",
            "",
            json!({"functionName": "nightly-report", "scheduleExpression": "@daily",
                   "scheduleTimezone": "Europe/Berlin", "catchupPolicy": "run_once",
                   "collection": "orders", "enabled": true, "logOkInvocations": true})
        )
    );

    c.schedules()
        .create(CreateScheduleInput {
            function_name: "f",
            schedule_expression: "0 * * * *",
            schedule_timezone: None,
            catchup_policy: None,
            collection: None,
            enabled: None,
            log_ok_invocations: None,
        })
        .await
        .unwrap();
    assert_eq!(
        last(&server).await.3,
        json!({"functionName": "f", "scheduleExpression": "0 * * * *"})
    );

    c.schedules()
        .update(
            id,
            UpdateScheduleInput {
                status: Some("paused"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last(&server).await,
        call(
            "PATCH",
            &format!("/@portal/forge/schedules/{id}"),
            "",
            json!({"status": "paused"})
        )
    );
}

#[tokio::test]
async fn schedules_delete_204_runs_and_run_now() {
    let id = "11111111-2222-3333-4444-555555555555";

    let (server, c) = setup(204, None).await;
    let r = c.schedules().delete(id).await.unwrap();
    assert_eq!(r.data, ());
    assert_eq!(
        last(&server).await,
        call(
            "DELETE",
            &format!("/@portal/forge/schedules/{id}"),
            "",
            Value::Null
        )
    );

    let run = json!({"id": "run_1", "triggerId": id, "functionName": "nightly-report",
                     "collection": null, "event": "schedule", "status": "ok", "attemptCount": 1,
                     "errorDetail": null, "errorOutput": null, "recordId": null,
                     "triggeredAt": "2026-10-26T00:00:00.000Z", "completedAt": null});
    let (server, c) = setup(
        200,
        Some(json!({"data": [run], "meta": {"cursor": {"next": "n2"}}})),
    )
    .await;
    let r = c
        .schedules()
        .runs(
            id,
            ListScheduleRunsOptions {
                limit: Some(5),
                cursor: Some("n1"),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.data[0].attempt_count, 1);
    assert_eq!(r.data[0].trigger_id, id);
    assert_eq!(r.meta.cursor.unwrap().next.as_deref(), Some("n2"));
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            &format!("/@portal/forge/schedules/{id}/runs"),
            "limit=5&cursor=n1",
            Value::Null
        )
    );

    // run-now: the body is whatever the gateway sends (202).
    let (server, c) = setup(202, Some(json!({"data": {"queued": true}, "meta": {}}))).await;
    let r = c.schedules().run_now(id).await.unwrap();
    assert_eq!(r.data, json!({"queued": true}));
    assert_eq!(
        last(&server).await,
        call(
            "POST",
            &format!("/@portal/forge/schedules/{id}/run-now"),
            "",
            Value::Null
        )
    );
}

#[tokio::test]
async fn schedules_validation_sends_nothing() {
    let (server, c) = setup(200, Some(json!({"data": [], "meta": {}}))).await;
    let s = c.schedules();
    for limit in [0, 201] {
        assert_invalid(
            s.list(ListSchedulesOptions {
                limit: Some(limit),
                ..Default::default()
            })
            .await
            .unwrap_err(),
        );
        assert_invalid(
            s.runs(
                "x",
                ListScheduleRunsOptions {
                    limit: Some(limit),
                    cursor: None,
                },
            )
            .await
            .unwrap_err(),
        );
    }
    assert_invalid(
        s.list(ListSchedulesOptions {
            status: Some("running"),
            ..Default::default()
        })
        .await
        .unwrap_err(),
    );
    assert_invalid(s.get("").await.unwrap_err());
    assert_invalid(
        s.update("x", UpdateScheduleInput::default())
            .await
            .unwrap_err(),
    );
    assert_invalid(
        s.update(
            "x",
            UpdateScheduleInput {
                catchup_policy: Some("run_twice"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err(),
    );
    assert_invalid(
        s.create(CreateScheduleInput {
            function_name: "f",
            schedule_expression: "@daily",
            schedule_timezone: None,
            catchup_policy: Some("sometimes"),
            collection: None,
            enabled: None,
            log_ok_invocations: None,
        })
        .await
        .unwrap_err(),
    );
    assert_invalid(s.delete("").await.unwrap_err());
    assert_eq!(sent(&server).await, 0);
}

// --- C. end users and role grants ---------------------------------------------------------

fn end_user() -> Value {
    json!({"id": "eu_YmGE3Z6nkv5i", "email": "user@example.com", "emailVerifiedAt": null,
           "status": "active", "createdAt": "2026-10-09T12:15:13.430Z",
           "lastSigninAt": "2026-10-21T06:39:02.321Z",
           "signInMethods": ["password", "magic_link"], "region": null})
}

#[tokio::test]
async fn end_users_list_and_get_wire() {
    let (server, c) = setup(
        200,
        Some(json!({"data": [end_user()], "meta": {"cursor": {"next": "c2"}}})),
    )
    .await;
    let r = c
        .end_users()
        .list(ListEndUsersOptions {
            status: Some("all"),
            q: Some("user@"),
            limit: Some(1),
            cursor: Some("c1"),
        })
        .await
        .unwrap();
    assert_eq!(r.data[0].id, "eu_YmGE3Z6nkv5i");
    assert_eq!(r.data[0].sign_in_methods, vec!["password", "magic_link"]);
    assert_eq!(r.data[0].email_verified_at, None);
    assert_eq!(r.meta.cursor.unwrap().next.as_deref(), Some("c2"));
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            "/@portal/end-users",
            "status=all&q=user%40&limit=1&cursor=c1",
            Value::Null
        )
    );

    let (server, c) = setup(200, Some(json!({"data": end_user(), "meta": {}}))).await;
    let u = c.end_users().get("eu_YmGE3Z6nkv5i").await.unwrap();
    assert_eq!(u.data.status, "active");
    assert_eq!(
        last(&server).await,
        call("GET", "/@portal/end-users/eu_YmGE3Z6nkv5i", "", Value::Null)
    );
}

#[tokio::test]
async fn role_grants_list_empty_fixture_unwraps_grants() {
    let (server, c) = setup(200, Some(fixture("end_user_role_grants_list_empty.json"))).await;
    let r = c
        .end_users()
        .role_grants()
        .list(ListRoleGrantsOptions {
            end_user_id: Some("eu_x"),
        })
        .await
        .unwrap();
    assert!(r.data.is_empty());
    assert!(r.meta.context.is_some());
    assert_eq!(
        last(&server).await,
        call(
            "GET",
            "/@portal/end-user-role-grants",
            "endUserId=eu_x",
            Value::Null
        )
    );
    c.end_users()
        .role_grants()
        .list(ListRoleGrantsOptions::default())
        .await
        .unwrap();
    assert_eq!(last(&server).await.2, "");
}

#[tokio::test]
async fn role_grants_list_with_rows() {
    let body = json!({"data": {"grants": [
        {"id": "eurg_AbCdEf123456", "endUserId": "eu_1", "role": "billing-manager",
         "grantedAt": "2026-10-26T17:59:56.815Z", "grantedBy": "usr_1"}]}, "meta": {}});
    let (_s, c) = setup(200, Some(body)).await;
    let r = c
        .end_users()
        .role_grants()
        .list(ListRoleGrantsOptions::default())
        .await
        .unwrap();
    assert_eq!(r.data.len(), 1);
    assert_eq!(r.data[0].end_user_id, "eu_1");
    assert_eq!(r.data[0].granted_by, "usr_1");
}

#[tokio::test]
async fn role_grants_grant_unwraps_grant() {
    let body = json!({"data": {"grant": {"id": "eurg_AbCdEf123456", "endUserId": "eu_1",
        "role": "billing-manager", "grantedAt": "t", "grantedBy": "usr_1"}}, "meta": {}});
    let (server, c) = setup(200, Some(body)).await;
    let r = c
        .end_users()
        .role_grants()
        .grant(GrantEndUserRoleInput {
            end_user_id: "eu_1",
            role: "billing-manager",
        })
        .await
        .unwrap();
    assert_eq!(r.data.id, "eurg_AbCdEf123456");
    assert_eq!(r.data.role, "billing-manager");
    assert_eq!(
        last(&server).await,
        call(
            "POST",
            "/@portal/end-user-role-grants",
            "",
            json!({"endUserId": "eu_1", "role": "billing-manager"})
        )
    );
}

#[tokio::test]
async fn role_grants_revoke_true_and_false_are_both_results() {
    let (server, c) = setup(
        200,
        Some(
            json!({"data": {"revoked": true, "revokedAt": "2026-10-26T18:00:00.000Z"}, "meta": {}}),
        ),
    )
    .await;
    let r = c
        .end_users()
        .role_grants()
        .revoke("eurg_AbCdEf123456")
        .await
        .unwrap();
    assert!(r.data.revoked);
    assert_eq!(
        r.data.revoked_at.as_deref(),
        Some("2026-10-26T18:00:00.000Z")
    );
    assert_eq!(
        last(&server).await,
        call(
            "DELETE",
            "/@portal/end-user-role-grants/eurg_AbCdEf123456",
            "",
            Value::Null
        )
    );

    let (_s, c) = setup(
        200,
        Some(json!({"data": {"revoked": false, "revokedAt": null}, "meta": {}})),
    )
    .await;
    let r = c
        .end_users()
        .role_grants()
        .revoke("eurg_nope")
        .await
        .unwrap();
    assert!(!r.data.revoked);
    assert_eq!(r.data.revoked_at, None);
}

#[tokio::test]
async fn end_users_validation_sends_nothing() {
    let (server, c) = setup(200, Some(json!({"data": [], "meta": {}}))).await;
    let eu = c.end_users();
    assert_invalid(
        eu.list(ListEndUsersOptions {
            limit: Some(0),
            ..Default::default()
        })
        .await
        .unwrap_err(),
    );
    assert_invalid(eu.get("").await.unwrap_err());
    assert_invalid(eu.get(&"e".repeat(65)).await.unwrap_err());
    let g = eu.role_grants();
    let long = "r".repeat(129);
    for (user, role) in [("", "r"), ("eu_1", ""), ("eu_1", long.as_str())] {
        assert_invalid(
            g.grant(GrantEndUserRoleInput {
                end_user_id: user,
                role,
            })
            .await
            .unwrap_err(),
        );
    }
    assert_invalid(g.revoke("").await.unwrap_err());
    assert_eq!(sent(&server).await, 0);
}

#[tokio::test]
async fn gateway_errors_come_back_as_kerror() {
    let body = json!({"code": "K9O_TRIGGER_NOT_FOUND", "message": "no such trigger",
                      "context": {"triggerId": "t"}, "recommendation": null});
    let (_s, c) = setup(404, Some(body)).await;
    let e = c.functions().triggers().delete("f", "t").await.unwrap_err();
    assert_eq!(e.code, "K9O_TRIGGER_NOT_FOUND");
    assert_eq!(e.status, Some(404));
}

#[tokio::test]
async fn no_project_slug_is_a_missing_context_error() {
    let server = MockServer::start().await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        ..Default::default()
    })
    .unwrap();
    let e = c.schedules().get("x").await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
    let e = c.end_users().get("x").await.unwrap_err();
    assert_eq!(e.code, "K9O_MISSING_PROJECT_CONTEXT");
}
