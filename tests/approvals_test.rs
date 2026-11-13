//! Approvals, approval policies and schema collections: exact wire shape per
//! call, response decoding, and every client-side validation error (no request
//! is sent when validation fails).

use k9o::{
    AddApprovalCommentInput, ApproveInput, BulkApprovalsInput, Client, ClientOpts,
    CreateApprovalPolicyInput, KError, ListApprovalCommentsOptions, ListApprovalsOptions,
    ListCollectionsOptions, RejectInput,
};
use serde_json::{json, Value};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup(data: Value) -> (MockServer, Client) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": data, "meta": {"cursor": {"next": "dGFza3M"}}})),
        )
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

async fn sent(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn assert_invalid(err: KError) {
    assert_eq!(err.code, "K9O_INVALID_FIELD_VALUE", "{err}");
}

fn pending() -> Value {
    json!({"id": "apr_1", "status": "pending_approval", "collection": "orders", "operation": "update",
           "makerId": "usr_1", "proposedPayload": {"amount": 5}, "targetRecordId": null, "createdAt": "t"})
}

#[tokio::test]
async fn approvals_wire_and_decoding() {
    let (server, c) = setup(pending()).await;

    let got = c
        .approvals()
        .approve("apr_1", ApproveInput::default())
        .await
        .unwrap();
    assert_eq!(got.data.status, "pending_approval");
    assert_eq!(got.data.target_record_id, None);
    assert_eq!(got.data.proposed_payload["amount"], 5);
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/pending-approvals/apr_1/approve".into(),
            "".into(),
            json!({})
        )
    );

    c.approvals()
        .approve("apr_1", ApproveInput { reason: Some("ok") })
        .await
        .unwrap();
    assert_eq!(last(&server).await.3, json!({"reason": "ok"}));

    c.approvals().get("apr_1").await.unwrap();
    assert_eq!(
        last(&server).await,
        (
            "GET".into(),
            "/@portal/pending-approvals/apr_1".into(),
            "".into(),
            Value::Null
        )
    );

    let _ = c.approvals().list(ListApprovalsOptions::default()).await;
    let (m, p, q, _) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str(), q.as_str()),
        ("GET", "/@portal/pending-approvals", "")
    );

    c.approvals()
        .reject("apr_1", RejectInput { reason: "dup" })
        .await
        .unwrap();
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/pending-approvals/apr_1/reject".into(),
            "".into(),
            json!({"reason": "dup"})
        )
    );

    let criteria = json!({});
    let _ = c
        .approvals()
        .bulk(BulkApprovalsInput {
            action: "approve",
            criteria: &criteria,
            reason: None,
        })
        .await;
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/pending-approvals/bulk".into(),
            "".into(),
            json!({"action": "approve", "criteria": {}})
        )
    );
}

#[tokio::test]
async fn approvals_list_query_and_bulk_result() {
    let (server, c) = setup(json!([pending()])).await;
    let listed = c
        .approvals()
        .list(ListApprovalsOptions {
            status: Some("approved"),
            limit: Some(200),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(listed.data.len(), 1);
    let q = last(&server).await.2;
    assert!(
        q.contains("status=approved") && q.contains("limit=200"),
        "{q}"
    );

    let (_s, c) = setup(
        json!({"attestationId": "apr_9", "action": "reject", "affectedCount": 2,
                              "affectedIds": ["apr_a", "apr_b"], "reason": "stale"}),
    )
    .await;
    let criteria = json!({"eq": ["change.table", "orders"]});
    let res = c
        .approvals()
        .bulk(BulkApprovalsInput {
            action: "reject",
            criteria: &criteria,
            reason: Some("stale"),
        })
        .await
        .unwrap();
    assert_eq!(res.data.affected_count, 2);
    assert_eq!(res.data.affected_ids, vec!["apr_a", "apr_b"]);
    assert_eq!(res.data.reason.as_deref(), Some("stale"));
}

#[tokio::test]
async fn approvals_validation_sends_nothing() {
    let (server, c) = setup(json!(null)).await;
    let criteria = json!({"eq": ["change.table", "orders"]});
    let null = Value::Null;
    let array = json!([1]);

    assert_invalid(
        c.approvals()
            .list(ListApprovalsOptions {
                status: None,
                limit: Some(0),
                ..Default::default()
            })
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .list(ListApprovalsOptions {
                status: None,
                limit: Some(201),
                ..Default::default()
            })
            .await
            .unwrap_err(),
    );
    assert_invalid(c.approvals().get("").await.unwrap_err());
    assert_invalid(
        c.approvals()
            .approve("", ApproveInput::default())
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .reject("apr_1", RejectInput { reason: "" })
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .reject("apr_1", RejectInput { reason: "  " })
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .reject("", RejectInput { reason: "x" })
            .await
            .unwrap_err(),
    );
    for (action, crit, reason) in [
        ("delete", &criteria, None),
        ("approve", &null, None),
        ("approve", &array, None),
        ("reject", &criteria, None),
        ("reject", &criteria, Some("  ")),
    ] {
        assert_invalid(
            c.approvals()
                .bulk(BulkApprovalsInput {
                    action,
                    criteria: crit,
                    reason,
                })
                .await
                .unwrap_err(),
        );
    }
    assert_invalid(
        c.schema()
            .collections()
            .list(ListCollectionsOptions {
                limit: Some(0),
                cursor: None,
            })
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.schema()
            .collections()
            .list(ListCollectionsOptions {
                limit: Some(500),
                cursor: None,
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(sent(&server).await, 0);
}

#[tokio::test]
async fn approval_policies_wire_and_validation() {
    let policy = json!({"id": "apo_1", "collection": "orders", "operation": "update", "action": "queue",
                        "predicate": {"gt": ["change.row.amount", 1000]}, "authoredBy": "usr_1", "createdAt": "t"});
    let (server, c) = setup(policy).await;
    let predicate = json!({"gt": ["change.row.amount", 1000]});
    let made = c
        .approval_policies()
        .create(CreateApprovalPolicyInput {
            collection: "orders",
            operation: "update",
            action: "queue",
            predicate: &predicate,
        })
        .await
        .unwrap();
    assert_eq!(
        (made.data.id.as_str(), made.data.action.as_str()),
        ("apo_1", "queue")
    );
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/approval-policies".into(),
            "".into(),
            json!({"collection": "orders", "operation": "update", "action": "queue", "predicate": predicate})
        )
    );
    let _ = c.approval_policies().list().await;
    assert_eq!(last(&server).await.0, "GET");
    assert_eq!(last(&server).await.1, "/@portal/approval-policies");
    let _ = c.approval_policies().delete("apo_1").await;
    assert_eq!(last(&server).await.0, "DELETE");
    assert_eq!(last(&server).await.1, "/@portal/approval-policies/apo_1");

    let before = sent(&server).await;
    let long64 = "a".repeat(64);
    let long63 = "a".repeat(63);
    let empty_obj = json!({});
    let null = Value::Null;
    let array = json!([]);
    let bad: Vec<(&str, &str, &str, &Value)> = vec![
        ("", "insert", "queue", &empty_obj),
        ("1orders", "insert", "queue", &empty_obj),
        ("or-ders", "insert", "queue", &empty_obj),
        (long64.as_str(), "insert", "queue", &empty_obj),
        ("orders", "upsert", "queue", &empty_obj),
        ("orders", "insert", "allow", &empty_obj),
        ("orders", "insert", "queue", &null),
        ("orders", "insert", "queue", &array),
    ];
    for (collection, operation, action, predicate) in bad {
        assert_invalid(
            c.approval_policies()
                .create(CreateApprovalPolicyInput {
                    collection,
                    operation,
                    action,
                    predicate,
                })
                .await
                .unwrap_err(),
        );
    }
    assert_invalid(c.approval_policies().delete("").await.unwrap_err());
    assert_eq!(sent(&server).await, before);

    // 63 characters is the longest accepted name.
    c.approval_policies()
        .create(CreateApprovalPolicyInput {
            collection: &long63,
            operation: "delete",
            action: "auto_approve",
            predicate: &empty_obj,
        })
        .await
        .unwrap();
    assert_eq!(sent(&server).await, before + 1);
}

#[tokio::test]
async fn schema_collections_wire_and_decoding() {
    let row = json!({"name": "orders", "rowCount": 120, "rowCountIsExact": true, "columnCount": 8,
                     "indexCount": 3, "lastMigrationSlug": null, "lastMigrationAppliedAt": null});
    let (server, c) = setup(json!([row])).await;
    let page = c
        .schema()
        .collections()
        .list(ListCollectionsOptions {
            limit: Some(2),
            cursor: Some("abc"),
        })
        .await
        .unwrap();
    assert_eq!(page.data[0].name, "orders");
    assert_eq!(page.data[0].row_count, 120);
    assert_eq!(page.data[0].last_migration_slug, None);
    assert_eq!(page.meta.cursor.unwrap().next.as_deref(), Some("dGFza3M"));
    let (m, p, q, _) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str()),
        ("GET", "/@portal/schema/collections")
    );
    assert!(q.contains("limit=2") && q.contains("cursor=abc"), "{q}");

    c.schema()
        .collections()
        .list(ListCollectionsOptions::default())
        .await
        .unwrap();
    assert_eq!(last(&server).await.2, "");
}

#[tokio::test]
async fn needs_project_slug() {
    let c = Client::new(ClientOpts {
        base_url: Some("http://127.0.0.1:9".into()),
        ..Default::default()
    })
    .unwrap();
    let err = c
        .approvals()
        .list(ListApprovalsOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_MISSING_PROJECT_CONTEXT");
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn serve(status: u16, body: Value) -> (MockServer, Client) {
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

#[tokio::test]
async fn approvals_list_first_page_exposes_next_cursor() {
    let (server, c) = serve(200, fixture("approvals_list_page1_200.json")).await;
    let page = c
        .approvals()
        .list(ListApprovalsOptions {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].id, "apr_7FjMvznAaVjl");
    let next = page.meta.cursor.unwrap().next.unwrap();
    assert!(next.starts_with("eyJ2"));
    assert_eq!(
        last(&server).await,
        (
            "GET".into(),
            "/@portal/pending-approvals".into(),
            "limit=1".into(),
            Value::Null
        )
    );
}

#[tokio::test]
async fn approvals_list_cursor_is_sent_and_last_page_has_null_next() {
    let (server, c) = serve(200, fixture("approvals_list_last_page_200.json")).await;
    let page = c
        .approvals()
        .list(ListApprovalsOptions {
            status: Some("approved"),
            limit: Some(1),
            cursor: Some("abc-_9"),
        })
        .await
        .unwrap();
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.meta.cursor.unwrap().next, None);
    assert_eq!(
        last(&server).await,
        (
            "GET".into(),
            "/@portal/pending-approvals".into(),
            "status=approved&limit=1&cursor=abc-_9".into(),
            Value::Null
        )
    );
}

#[tokio::test]
async fn approvals_list_invalid_cursor_is_a_400_kerror() {
    let (server, c) = serve(400, fixture("approvals_list_invalid_cursor_400.json")).await;
    let err = c
        .approvals()
        .list(ListApprovalsOptions {
            cursor: Some("abc!"),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_INVALID_CURSOR");
    assert_eq!(err.status, Some(400));
    assert_eq!(sent(&server).await, 1);
    assert_eq!(last(&server).await.2, "cursor=abc%21");
}

#[tokio::test]
async fn approvals_bulk_reports_failed_rows() {
    let (server, c) = serve(200, fixture("approvals_bulk_failed_200.json")).await;
    let criteria = json!({"eq": ["change.table", "orders"]});
    let res = c
        .approvals()
        .bulk(BulkApprovalsInput {
            action: "approve",
            criteria: &criteria,
            reason: None,
        })
        .await
        .unwrap();
    assert_eq!(res.data.affected_count, 1);
    assert_eq!(res.data.affected_ids, vec!["apr_A"]);
    assert_eq!(res.data.failed.len(), 1);
    assert_eq!(res.data.failed[0].id, "apr_B");
    assert_eq!(res.data.failed[0].code, "K9O_STATE_CONFLICT");
    assert_eq!(
        res.data.failed[0].reason,
        "Pending change 'apr_B' was decided concurrently"
    );
    assert_eq!(res.data.reason, None);
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/pending-approvals/bulk".into(),
            "".into(),
            json!({"action": "approve", "criteria": {"eq": ["change.table", "orders"]}})
        )
    );
}

#[tokio::test]
async fn approvals_bulk_empty_failed_and_absent_failed_decode_to_empty() {
    let (_s, c) = serve(200, fixture("approvals_bulk_none_200.json")).await;
    let criteria = json!({"eq": ["change.table", "__none__"]});
    let res = c
        .approvals()
        .bulk(BulkApprovalsInput {
            action: "reject",
            criteria: &criteria,
            reason: Some("probe: matches nothing"),
        })
        .await
        .unwrap();
    assert_eq!(res.data.affected_count, 0);
    assert!(res.data.affected_ids.is_empty() && res.data.failed.is_empty());

    let (_s, c) = setup(
        json!({"attestationId": "apr_9", "action": "approve", "affectedCount": 0, "reason": null}),
    )
    .await;
    let res = c
        .approvals()
        .bulk(BulkApprovalsInput {
            action: "approve",
            criteria: &criteria,
            reason: None,
        })
        .await
        .unwrap();
    assert!(res.data.failed.is_empty());
}

fn comment(id: &str, visibility: &str) -> Value {
    json!({"id": id, "caseId": "apr_1", "authorId": "usr_2", "visibility": visibility,
           "body": "check the amount", "createdAt": "2026-11-13T10:00:00.000Z"})
}

#[tokio::test]
async fn approval_comments_add_wire_and_decoding() {
    let (server, c) = setup(comment("acm_1", "approvers_only")).await;
    let got = c
        .approvals()
        .comments()
        .add(
            "apr_1",
            AddApprovalCommentInput {
                body: "check the amount",
                visibility: "approvers_only",
            },
        )
        .await
        .unwrap();
    assert_eq!(got.data.id, "acm_1");
    assert_eq!(got.data.case_id, "apr_1");
    assert_eq!(got.data.author_id, "usr_2");
    assert_eq!(got.data.visibility, "approvers_only");
    assert_eq!(got.data.created_at, "2026-11-13T10:00:00.000Z");
    assert_eq!(
        last(&server).await,
        (
            "POST".into(),
            "/@portal/pending-approvals/apr_1/comments".into(),
            "".into(),
            json!({"body": "check the amount", "visibility": "approvers_only"})
        )
    );
}

#[tokio::test]
async fn approval_comments_add_validates_before_sending() {
    let (server, c) = setup(comment("acm_1", "everyone")).await;
    let ok =
        |body: &'static str, visibility: &'static str| AddApprovalCommentInput { body, visibility };
    assert_invalid(
        c.approvals()
            .comments()
            .add("", ok("x", "everyone"))
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .comments()
            .add("apr_1", ok("", "everyone"))
            .await
            .unwrap_err(),
    );
    let long = "a".repeat(4001);
    assert_invalid(
        c.approvals()
            .comments()
            .add(
                "apr_1",
                AddApprovalCommentInput {
                    body: &long,
                    visibility: "everyone",
                },
            )
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .comments()
            .add("apr_1", ok("x", ""))
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .comments()
            .add("apr_1", ok("x", "Everyone"))
            .await
            .unwrap_err(),
    );
    assert_eq!(sent(&server).await, 0);

    // 4000 characters (multi-byte included) is accepted.
    let edge = "é".repeat(4000);
    c.approvals()
        .comments()
        .add(
            "apr_1",
            AddApprovalCommentInput {
                body: &edge,
                visibility: "everyone",
            },
        )
        .await
        .unwrap();
    assert_eq!(sent(&server).await, 1);
}

#[tokio::test]
async fn approval_comments_list_wire_and_page_decoding() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [comment("acm_1", "everyone"), comment("acm_2", "approvers_only")],
            "nextCursor": "c2",
            "meta": {}
        })))
        .mount(&server)
        .await;
    let c = Client::new(ClientOpts {
        base_url: Some(server.uri()),
        project_slug: Some("portal".into()),
        ..Default::default()
    })
    .unwrap();

    let page = c
        .approvals()
        .comments()
        .list(
            "apr_1",
            ListApprovalCommentsOptions {
                limit: Some(2),
                cursor: Some("c1"),
            },
        )
        .await
        .unwrap();
    assert_eq!(page.data.comments.len(), 2);
    assert_eq!(page.data.comments[1].id, "acm_2");
    assert_eq!(page.data.next_cursor.as_deref(), Some("c2"));
    let (m, p, q, b) = last(&server).await;
    assert_eq!(
        (m.as_str(), p.as_str(), q.as_str(), b),
        (
            "GET",
            "/@portal/pending-approvals/apr_1/comments",
            "limit=2&cursor=c1",
            Value::Null
        )
    );

    c.approvals()
        .comments()
        .list("apr_1", ListApprovalCommentsOptions::default())
        .await
        .unwrap();
    assert_eq!(last(&server).await.2, "");

    assert_invalid(
        c.approvals()
            .comments()
            .list(
                "apr_1",
                ListApprovalCommentsOptions {
                    limit: Some(201),
                    cursor: None,
                },
            )
            .await
            .unwrap_err(),
    );
    assert_invalid(
        c.approvals()
            .comments()
            .list("", ListApprovalCommentsOptions::default())
            .await
            .unwrap_err(),
    );
    assert_eq!(sent(&server).await, 2);
}

#[tokio::test]
async fn approval_comments_last_page_and_error_mapping() {
    let (_s, c) = serve(200, json!({"data": [], "nextCursor": null, "meta": {}})).await;
    let page = c
        .approvals()
        .comments()
        .list("apr_1", ListApprovalCommentsOptions::default())
        .await
        .unwrap();
    assert!(page.data.comments.is_empty() && page.data.next_cursor.is_none());

    let (_s, c) = serve(
        409,
        json!({"code": "K9O_APPROVAL_COMMENT_CLOSED", "message": "closed", "context": {"caseId": "apr_1", "status": "approved"}}),
    )
    .await;
    let err = c
        .approvals()
        .comments()
        .add(
            "apr_1",
            AddApprovalCommentInput {
                body: "late",
                visibility: "everyone",
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "K9O_APPROVAL_COMMENT_CLOSED");
}
