//! Contract test: every public method, called with representative arguments,
//! puts exactly the request the gateway route reads on the wire (verb, path,
//! query and body). The expected requests are the gateway's own: routes and
//! body schemas from `services/gateway/openapi.json` (and, for routes whose
//! OpenAPI entry declares no body, the handler named in the comments).
//!
//! Set `K9O_CONTRACT_DUMP=<file>` to also write each request as one JSON line,
//! which a script can check against the OpenAPI.

use k9o::{
    ApiTokenScopeManifest, ApproveInput, AuditQueryOpts, BulkApprovalsInput, Client, ClientOpts,
    CreateApiTokenInput, CreateApprovalPolicyInput, CreateBucketOptions, CreateIdpInput,
    CreatePolicyInput, CreateProjectInput, CreateSecretInput, DeployFunctionInput, EmbedInput,
    GenerateOptions, GrantRoleInput, InvokeOptions, ListApprovalsOptions, ListCollectionsOptions,
    ListFunctionsOptions, ListSecretsInput, LlmInvokeInput, LlmInvokeParams, Migration,
    OrgAuditQueryOpts, RefreshTokenInput, RejectInput, SetSmtpInput, SignInInput, SignOutInput,
    SignUpInput, SignedUrlOptions, UpdateBucketOptions, UpdateIdpInput, UpdatePolicyInput,
    UpdateProjectInput, UpdateSecretInput, UploadOptions, ValidatePolicyInput,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::sync::Arc;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// What the route reads.
struct Expect<'a> {
    method: &'a str,
    /// Escaped path, as sent.
    path: &'a str,
    query: &'a [(&'a str, &'a str)],
    /// Exact JSON body; `None` = no body.
    body: Option<Value>,
    /// Raw (non-JSON) body bytes, with the required content-type prefix.
    raw: Option<(&'a [u8], &'a str)>,
    /// A multipart body: only the content-type prefix is checked.
    multipart: bool,
    /// Body contract for a route whose OpenAPI entry declares none: [required, properties|null].
    contract: Option<Value>,
    /// The call asks the locate door first, with this email and no credential, then makes the request above.
    door: Option<&'a str>,
}

fn e<'a>(method: &'a str, path: &'a str) -> Expect<'a> {
    Expect {
        method,
        path,
        query: &[],
        body: None,
        raw: None,
        multipart: false,
        contract: None,
        door: None,
    }
}

fn client(server: &MockServer) -> Arc<Client> {
    Arc::new(
        Client::new(ClientOpts {
            base_url: Some(server.uri()),
            org_slug: Some("acme".into()),
            project_slug: Some("portal".into()),
            ..Default::default()
        })
        .unwrap(),
    )
}

fn dump(id: &str, req: &Request, x: &Expect<'_>) {
    let Ok(file) = std::env::var("K9O_CONTRACT_DUMP") else {
        return;
    };
    let mut keys: Vec<String> = req.url.query_pairs().map(|(k, _)| k.to_string()).collect();
    keys.sort();
    let mut entry = json!({
        "id": id,
        "method": req.method.as_str(),
        "path": req.url.path(),
        "query": keys,
        "raw": x.raw.is_some() || x.multipart,
    });
    if x.raw.is_none() && !x.multipart && !req.body.is_empty() {
        entry["body"] = serde_json::from_slice(&req.body).unwrap();
    }
    if is_pending(req.method.as_str(), req.url.path()) {
        entry["pending"] = json!(true);
    }
    if let Some(c) = &x.contract {
        entry["contract"] = c.clone();
    }
    if id.starts_with("kibo.vault.secrets.") {
        entry["extraQuery"] = json!(["region"]);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .unwrap();
    // One write call per line: the four tests run in parallel and append to the same file.
    f.write_all(format!("{}\n", entry).as_bytes()).unwrap();
}

async fn check<F, Fut>(id: &str, x: Expect<'_>, f: F)
where
    F: FnOnce(Arc<Client>) -> Fut,
    Fut: Future<Output = ()>,
{
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(|req: &Request| {
            let p = req.url.path();
            if p.ends_with("/invoke-stream")
                || p.ends_with("/realtime/subscribe")
                || p.ends_with("/subscribe")
            {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"data": null, "meta": {}}))
            }
        })
        .mount(&server)
        .await;
    f(client(&server)).await;
    let mut reqs = server.received_requests().await.unwrap();
    if let Some(email) = x.door {
        // The locate door comes first: only the email, no credential. The answer here has no `data`, so the base is used.
        assert!(!reqs.is_empty(), "{id}: no door request");
        let door = reqs.remove(0);
        assert_eq!(door.method.as_str(), "POST", "{id}: door method");
        assert_eq!(door.url.path(), "/@portal/gate/locate", "{id}: door path");
        assert_eq!(
            serde_json::from_slice::<Value>(&door.body).unwrap(),
            json!({ "email": email }),
            "{id}: door body"
        );
        assert!(
            door.headers.get("authorization").is_none(),
            "{id}: door credential"
        );
        dump("gate.locate", &door, &e("POST", "/@portal/gate/locate"));
    }
    assert_eq!(
        reqs.len(),
        1,
        "{id}: want exactly one request, got {}",
        reqs.len()
    );
    let req = &reqs[0];
    assert_eq!(req.method.as_str(), x.method, "{id}: method");
    assert_eq!(req.url.path(), x.path, "{id}: path");
    let got: BTreeMap<String, String> = req
        .url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let want: BTreeMap<String, String> = x
        .query
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(got, want, "{id}: query");
    if let Some((bytes, ct)) = x.raw {
        assert_eq!(req.body, bytes, "{id}: raw body");
        let have = req
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(have.starts_with(ct), "{id}: content-type {have:?}");
    } else if x.multipart {
        let have = req
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(
            have.starts_with("multipart/form-data"),
            "{id}: content-type {have:?}"
        );
    } else {
        match &x.body {
            Some(want) => {
                let have: Value = serde_json::from_slice(&req.body)
                    .unwrap_or_else(|_| panic!("{id}: body is not JSON"));
                assert_eq!(&have, want, "{id}: body");
            }
            None => assert!(
                req.body.is_empty(),
                "{id}: unexpected body {:?}",
                String::from_utf8_lossy(&req.body)
            ),
        }
    }
    dump(id, req, &x);
}

/// Routes the SDK already calls that are not in `services/gateway/openapi.json` yet. They are exempt from the
/// OpenAPI route check (the dump marks them `"pending"`) until the route lands; `pending_routes_are_still_pending`
/// fails once one appears in the OpenAPI, and the entry must then be removed.
const PENDING_ROUTES: &[(&str, &str)] = &[];

fn is_pending(method: &str, path: &str) -> bool {
    let path = match path.strip_prefix("/@portal/") {
        Some(rest) => format!("/@{{projectSlug}}/{rest}"),
        None => path.to_string(),
    };
    let key = format!("{method} {path}");
    PENDING_ROUTES.iter().any(|(k, _)| *k == key)
}

#[test]
fn pending_routes_are_still_pending() {
    let file = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../services/gateway/openapi.json"
    );
    let Ok(raw) = std::fs::read_to_string(file) else {
        return; // the gateway OpenAPI is not next to this package
    };
    let spec: Value = serde_json::from_str(&raw).unwrap();
    for (key, _) in PENDING_ROUTES {
        let (m, p) = key.split_once(' ').unwrap();
        assert!(
            spec["paths"][p][m.to_lowercase()].is_null(),
            "{key} is in the OpenAPI now: remove it from PENDING_ROUTES"
        );
    }
}

fn policy_doc() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("collections".into(), json!({}));
    m
}

#[tokio::test]
async fn contract_projects_functions_policies_audit() {
    check("projects.list", e("GET", "/projects"), |c| async move {
        let _ = c.projects().list().await;
    })
    .await;
    check(
        "projects.create",
        Expect {
            body: Some(
                json!({"slug":"demo","displayName":"Demo","custodyMode":"PLATFORM_MANAGED"}),
            ),
            ..e("POST", "/projects")
        },
        |c| async move {
            let _ = c
                .projects()
                .create(CreateProjectInput {
                    display_name: "Demo",
                    slug: Some("demo"),
                    custody_mode: Some("PLATFORM_MANAGED"),
                    ..Default::default()
                })
                .await;
        },
    )
    .await;
    check("projects.get", e("GET", "/projects/demo"), |c| async move {
        let _ = c.projects().get("demo").await;
    })
    .await;
    check(
        "projects.update",
        Expect {
            body: Some(json!({"displayName":"Renamed"})),
            ..e("PATCH", "/projects/demo")
        },
        |c| async move {
            let _ = c
                .projects()
                .update(
                    "demo",
                    UpdateProjectInput {
                        display_name: Some("Renamed"),
                        ..Default::default()
                    },
                )
                .await;
        },
    )
    .await;

    check(
        "functions.uploadArtifact",
        Expect {
            raw: Some((&[0, 97, 115, 109], "application/wasm")),
            ..e("POST", "/@portal/functions/artifacts")
        },
        |c| async move {
            let _ = c
                .functions()
                .upload_artifact(vec![0, 97, 115, 109], "wasm")
                .await;
        },
    )
    .await;
    check(
        "functions.deploy",
        Expect {
            body: Some(json!({"name":"hello","runtime":"bun","tier":"sandbox","artifactKey":"functions/p/abc.tar","version":"1"})),
            ..e("POST", "/@portal/functions")
        },
        |c| async move {
            let _ = c
                .functions()
                .deploy(DeployFunctionInput { name: "hello", runtime: "bun", tier: "sandbox", artifact_key: "functions/p/abc.tar", version: "1", description: None })
                .await;
        },
    )
    .await;
    check(
        "functions.list",
        Expect {
            query: &[("limit", "10"), ("status", "active")],
            ..e("GET", "/@portal/functions")
        },
        |c| async move {
            let _ = c
                .functions()
                .list(ListFunctionsOptions {
                    limit: Some(10),
                    status: Some("active"),
                    ..Default::default()
                })
                .await;
        },
    )
    .await;
    check(
        "functions.get",
        e("GET", "/@portal/functions/hello"),
        |c| async move {
            let _ = c.functions().get("hello").await;
        },
    )
    .await;
    check(
        "functions.delete",
        e("DELETE", "/@portal/functions/hello"),
        |c| async move {
            let _ = c.functions().delete("hello").await;
        },
    )
    .await;
    check(
        "functions.invoke",
        Expect {
            body: Some(json!({"payload":{"a":1}})),
            ..e("POST", "/@portal/functions/hello/invoke")
        },
        |c| async move {
            let _ = c
                .functions()
                .invoke("hello", Some(&json!({"a":1})), None)
                .await;
        },
    )
    .await;
    check(
        "functions.invoke.options",
        Expect {
            body: Some(json!({"background":true,"onBehalfOf":"u_1"})),
            ..e("POST", "/@portal/functions/hello/invoke")
        },
        |c| async move {
            let opts = InvokeOptions {
                background: Some(true),
                on_behalf_of: Some("u_1"),
                ..Default::default()
            };
            let _ = c.functions().invoke("hello", None, Some(&opts)).await;
        },
    )
    .await;

    check(
        "policies.current",
        e("GET", "/@portal/policy"),
        |c| async move {
            let _ = c.policies().current().await;
        },
    )
    .await;
    check(
        "policies.list",
        Expect {
            query: &[("status", "published")],
            ..e("GET", "/@portal/policies")
        },
        |c| async move {
            let _ = c.policies().list(Some("published")).await;
        },
    )
    .await;
    check(
        "policies.create",
        Expect {
            body: Some(json!({"policy":{"collections":{}},"editorMode":"json"})),
            ..e("POST", "/@portal/policies")
        },
        |c| async move {
            let doc = policy_doc();
            let _ = c
                .policies()
                .create(CreatePolicyInput {
                    policy: &doc,
                    rules: None,
                    editor_mode: Some("json"),
                })
                .await;
        },
    )
    .await;
    check(
        "policies.validate",
        Expect {
            body: Some(json!({"policy":{"collections":{}}})),
            ..e("POST", "/@portal/policies/validate")
        },
        |c| async move {
            let doc = policy_doc();
            let _ = c
                .policies()
                .validate(ValidatePolicyInput {
                    policy: &doc,
                    rules: None,
                })
                .await;
        },
    )
    .await;
    check(
        "policies.get",
        e("GET", "/@portal/policies/pol_1"),
        |c| async move {
            let _ = c.policies().get("pol_1").await;
        },
    )
    .await;
    check(
        "policies.update",
        Expect {
            body: Some(json!({"policy":{"collections":{}}})),
            ..e("PATCH", "/@portal/policies/pol_1")
        },
        |c| async move {
            let doc = policy_doc();
            let _ = c
                .policies()
                .update(
                    "pol_1",
                    UpdatePolicyInput {
                        policy: &doc,
                        rules: None,
                        editor_mode: None,
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "policies.delete",
        e("DELETE", "/@portal/policies/pol_1"),
        |c| async move {
            let _ = c.policies().delete("pol_1").await;
        },
    )
    .await;
    check(
        "policies.publish",
        e("POST", "/@portal/policies/pol_1/publish"),
        |c| async move {
            let _ = c.policies().publish("pol_1").await;
        },
    )
    .await;

    check(
        "audit.query",
        Expect {
            query: &[("limit", "10")],
            ..e("GET", "/@portal/audit-log")
        },
        |c| async move {
            let _ = c
                .audit()
                .query(AuditQueryOpts {
                    limit: Some(10),
                    ..Default::default()
                })
                .await;
        },
    )
    .await;
    check(
        "audit.query.filters",
        Expect {
            query: &[
                ("eventType", "auth.sign_in"),
                ("actorId", "u_1"),
                ("after", "2026-01-01"),
                ("before", "2026-02-01"),
                ("cursor", "cur"),
            ],
            ..e("GET", "/@portal/audit-log")
        },
        |c| async move {
            let opts = AuditQueryOpts {
                event_type: Some("auth.sign_in"),
                actor_id: Some("u_1"),
                after: Some("2026-01-01"),
                before: Some("2026-02-01"),
                cursor: Some("cur"),
                ..Default::default()
            };
            let _ = c.audit().query(opts).await;
        },
    )
    .await;
}

#[tokio::test]
async fn contract_orgs_org_me() {
    check("orgs.list", e("GET", "/orgs"), |c| async move {
        let _ = c.orgs().list().await;
    })
    .await;
    check("orgs.get", e("GET", "/orgs/acme"), |c| async move {
        let _ = c.orgs().get("acme").await;
    })
    .await;
    check(
        "orgs.restore",
        e("POST", "/orgs/org_1/restore"),
        |c| async move {
            let _ = c.orgs().restore("org_1").await;
        },
    )
    .await;
    check("orgs.current", e("GET", "/org"), |c| async move {
        let _ = c.orgs().current().await;
    })
    .await;
    check("org.get", e("GET", "/org"), |c| async move {
        let _ = c.org().get().await;
    })
    .await;
    check(
        "org.members.list",
        Expect {
            query: &[("status", "active")],
            ..e("GET", "/members")
        },
        |c| async move {
            let _ = c.org().members().list(Some("active")).await;
        },
    )
    .await;
    check(
        "org.members.invite",
        Expect {
            body: Some(json!({"email":"a@b.co","role":"ORG_MEMBER"})),
            ..e("POST", "/members")
        },
        |c| async move {
            let _ = c.org().members().invite("a@b.co", "ORG_MEMBER").await;
        },
    )
    .await;
    check(
        "org.members.remove",
        e("DELETE", "/members/u_1"),
        |c| async move {
            let _ = c.org().members().remove("u_1").await;
        },
    )
    .await;
    check("org.groups.list", e("GET", "/groups"), |c| async move {
        let _ = c.org().groups().list().await;
    })
    .await;
    check(
        "org.groups.get",
        e("GET", "/groups/grp_1"),
        |c| async move {
            let _ = c.org().groups().get("grp_1").await;
        },
    )
    .await;
    check(
        "org.auditLog.query",
        Expect {
            query: &[("type", "data.write.org.role_granted"), ("limit", "10")],
            ..e("GET", "/audit-log")
        },
        |c| async move {
            let _ = c
                .org()
                .audit_log()
                .query(OrgAuditQueryOpts {
                    event_type: Some("data.write.org.role_granted"),
                    limit: Some(10),
                })
                .await;
        },
    )
    .await;
    check(
        "org.gate.roles.list",
        e("GET", "/role-grants"),
        |c| async move {
            let _ = c.org().gate().roles().list().await;
        },
    )
    .await;
    check(
        "org.gate.roles.grant",
        Expect {
            body: Some(json!({"userId":"u_1","role":"ORG_ADMIN"})),
            ..e("POST", "/role-grants")
        },
        |c| async move {
            let _ = c
                .org()
                .gate()
                .roles()
                .grant(GrantRoleInput {
                    user_id: "u_1",
                    role: "ORG_ADMIN",
                    decision_reason: None,
                })
                .await;
        },
    )
    .await;
    check(
        "org.gate.roles.get",
        e("GET", "/role-grants/g_1"),
        |c| async move {
            let _ = c.org().gate().roles().get("g_1").await;
        },
    )
    .await;
    check(
        "org.gate.roles.revoke",
        e("DELETE", "/role-grants/g_1"),
        |c| async move {
            let _ = c.org().gate().roles().revoke("g_1", None).await;
        },
    )
    .await;
    check(
        "org.gate.roles.revoke.reason",
        Expect {
            body: Some(json!({"decisionReason":"left the company"})),
            ..e("DELETE", "/role-grants/g_1")
        },
        |c| async move {
            let _ = c
                .org()
                .gate()
                .roles()
                .revoke("g_1", Some("left the company"))
                .await;
        },
    )
    .await;
    check(
        "org.gate.idpProviders.list",
        e("GET", "/identity-providers"),
        |c| async move {
            let _ = c.org().gate().idp_providers().list().await;
        },
    )
    .await;
    check(
        "org.gate.idpProviders.create",
        Expect {
            body: Some(json!({"idpProvider":"saml","displayName":"Okta","clientId":"cid","clientSecretRef":"ref_1","tierGating":"free_tier_ok"})),
            ..e("POST", "/identity-providers")
        },
        |c| async move {
            let input = CreateIdpInput {
                idp_provider: "saml",
                display_name: "Okta",
                client_id: "cid",
                client_secret_ref: "ref_1",
                tier_gating: "free_tier_ok",
                redirect_uri: None,
                metadata_url: None,
                attribute_map: None,
                enabled: None,
            };
            let _ = c.org().gate().idp_providers().create(input).await;
        },
    )
    .await;
    check(
        "org.gate.idpProviders.get",
        e("GET", "/identity-providers/idp_1"),
        |c| async move {
            let _ = c.org().gate().idp_providers().get("idp_1").await;
        },
    )
    .await;
    check(
        "org.gate.idpProviders.configure",
        Expect {
            body: Some(json!({"enabled":true})),
            ..e("PATCH", "/identity-providers/idp_1")
        },
        |c| async move {
            let _ = c
                .org()
                .gate()
                .idp_providers()
                .configure(
                    "idp_1",
                    UpdateIdpInput {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "org.gate.idpProviders.delete",
        e("DELETE", "/identity-providers/idp_1"),
        |c| async move {
            let _ = c.org().gate().idp_providers().delete("idp_1").await;
        },
    )
    .await;
    check(
        "org.smtp.set",
        Expect {
            body: Some(json!({"smtpConfig":{"host":"smtp.x.io","port":587,"username":"u","passwordRef":"ref_1","fromAddress":"a@b.co","fromName":"A"}})),
            ..e("PATCH", "/org")
        },
        |c| async move {
            let input = SetSmtpInput { host: "smtp.x.io", port: 587, username: "u", password_ref: "ref_1", from_address: "a@b.co", from_name: Some("A") };
            let _ = c.org().smtp().set(&input).await;
        },
    )
    .await;
    check(
        "org.smtp.test",
        Expect {
            body: Some(json!({"to":"a@b.co"})),
            ..e("POST", "/org/smtp/test-email")
        },
        |c| async move {
            let _ = c.org().smtp().test("a@b.co", None).await;
        },
    )
    .await;
    check(
        "org.smtp.unset",
        Expect {
            body: Some(json!({"smtpConfig":null})),
            ..e("PATCH", "/org")
        },
        |c| async move {
            let _ = c.org().smtp().unset().await;
        },
    )
    .await;
    check(
        "org.defaultLLM.get",
        e("GET", "/default-llm"),
        |c| async move {
            let _ = c.org().default_llm_get().await;
        },
    )
    .await;
    check(
        "org.defaultLLM.set",
        Expect {
            body: Some(json!({"modelId":"llama-3-8b"})),
            ..e("PATCH", "/default-llm")
        },
        |c| async move {
            let _ = c.org().default_llm_set("llama-3-8b").await;
        },
    )
    .await;

    check("me.profile.get", e("GET", "/me"), |c| async move {
        let _ = c.me().profile().get().await;
    })
    .await;
    check(
        "me.profile.update",
        Expect {
            body: Some(json!({"displayName":"A"})),
            ..e("PATCH", "/me")
        },
        |c| async move {
            let _ = c.me().profile().update("A").await;
        },
    )
    .await;
    check(
        "me.sessions.list",
        e("GET", "/me/sessions"),
        |c| async move {
            let _ = c.me().sessions().list().await;
        },
    )
    .await;
    check(
        "me.sessions.revoke",
        e("POST", "/auth/sessions/s_1/revoke"),
        |c| async move {
            let _ = c.me().sessions().revoke("s_1").await;
        },
    )
    .await;
    check(
        "me.sessions.revokeAll",
        e("POST", "/me/sessions/revoke-all"),
        |c| async move {
            let _ = c.me().sessions().revoke_all().await;
        },
    )
    .await;
    check(
        "me.apiTokens.list",
        e("GET", "/me/api-tokens"),
        |c| async move {
            let _ = c.me().api_tokens().list().await;
        },
    )
    .await;
    check(
        "me.apiTokens.create",
        Expect {
            body: Some(json!({"label":"ci","scopeManifest":{"tier":"readonly","projects":["portal"]},"ttlSeconds":3600})),
            ..e("POST", "/me/api-tokens")
        },
        |c| async move {
            let input = CreateApiTokenInput {
                label: "ci",
                scope_manifest: ApiTokenScopeManifest { tier: Some("readonly"), projects: Some(vec!["portal"]), ..Default::default() },
                ttl_seconds: Some(3600),
            };
            let _ = c.me().api_tokens().create(input).await;
        },
    )
    .await;
    check(
        "me.apiTokens.delete",
        e("DELETE", "/me/api-tokens/pat_1"),
        |c| async move {
            let _ = c.me().api_tokens().delete("pat_1").await;
        },
    )
    .await;
}

fn init_migration() -> Migration {
    Migration {
        version: 1,
        slug: "init".into(),
        up: vec![],
        down: vec![],
        policy: None,
    }
}

#[tokio::test]
async fn contract_migrations_storage_ai_compute_studio() {
    check(
        "migrations.list",
        e("GET", "/@portal/migrations"),
        |c| async move {
            let _ = c.migrations().list().await;
        },
    )
    .await;
    check(
        "migrations.get",
        e("GET", "/@portal/migrations/1"),
        |c| async move {
            let _ = c.migrations().get("1").await;
        },
    )
    .await;
    // apply / queue: routes/migrations.ts parseApplyBody reads `{ migrations: [...] }` and nothing else.
    check(
        "migrations.apply",
        Expect {
            body: Some(json!({"migrations":[{"version":1,"slug":"init","up":[],"down":[]}]})),
            contract: Some(json!([["migrations"], ["migrations"]])),
            ..e("POST", "/@portal/migrations/apply")
        },
        |c| async move {
            let _ = c.migrations().apply(&[init_migration()]).await;
        },
    )
    .await;
    check(
        "migrations.queue",
        Expect {
            body: Some(json!({"migrations":[{"version":1,"slug":"init","up":[],"down":[]}]})),
            contract: Some(json!([["migrations"], ["migrations"]])),
            ..e("POST", "/@portal/migrations/queue")
        },
        |c| async move {
            let _ = c.migrations().queue(&[init_migration()]).await;
        },
    )
    .await;
    check(
        "migrations.listPending",
        e("GET", "/@portal/migrations/pending"),
        |c| async move {
            let _ = c.migrations().list_pending().await;
        },
    )
    .await;
    check(
        "migrations.applyPending",
        e("POST", "/@portal/migrations/pending/pm_1/apply"),
        |c| async move {
            let _ = c.migrations().apply_pending("pm_1").await;
        },
    )
    .await;
    // reject: the handler reads an optional `reason` string.
    check(
        "migrations.rejectPending",
        Expect {
            body: Some(json!({"reason":"no"})),
            contract: Some(json!([[], ["reason"]])),
            ..e("POST", "/@portal/migrations/pending/pm_1/reject")
        },
        |c| async move {
            let _ = c.migrations().reject_pending("pm_1", Some("no")).await;
        },
    )
    .await;
    check(
        "migrations.deletePending",
        e("DELETE", "/@portal/migrations/pending/pm_1"),
        |c| async move {
            let _ = c.migrations().delete_pending("pm_1").await;
        },
    )
    .await;

    check(
        "storage.listBuckets",
        e("GET", "/@portal/storage/buckets"),
        |c| async move {
            let _ = c.storage().list_buckets().await;
        },
    )
    .await;
    check(
        "storage.getBucket",
        e("GET", "/@portal/storage/buckets/docs"),
        |c| async move {
            let _ = c.storage().get_bucket("docs").await;
        },
    )
    .await;
    check(
        "storage.createBucket",
        Expect {
            body: Some(json!({"name":"docs","public":false})),
            ..e("POST", "/@portal/storage/buckets")
        },
        |c| async move {
            let _ = c
                .storage()
                .create_bucket(
                    "docs",
                    CreateBucketOptions {
                        public: Some(false),
                        ..Default::default()
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "storage.updateBucket",
        Expect {
            body: Some(json!({"public":true})),
            ..e("PATCH", "/@portal/storage/buckets/docs")
        },
        |c| async move {
            let _ = c
                .storage()
                .update_bucket(
                    "docs",
                    UpdateBucketOptions {
                        public: Some(true),
                        ..Default::default()
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "storage.deleteBucket",
        Expect {
            query: &[("empty", "true")],
            ..e("DELETE", "/@portal/storage/buckets/docs")
        },
        |c| async move {
            let _ = c.storage().delete_bucket("docs", true).await;
        },
    )
    .await;
    check(
        "storage.object.upload",
        Expect {
            multipart: true,
            ..e("POST", "/@portal/storage/buckets/docs/objects")
        },
        |c| async move {
            let _ = c
                .storage()
                .from("docs")
                .upload("a.txt", "hello", UploadOptions::default())
                .await;
        },
    )
    .await;
    check(
        "storage.object.createSignedUrl",
        Expect {
            body: Some(json!({"path":"a.txt","expiresIn":600})),
            ..e("POST", "/@portal/storage/buckets/docs/objects/sign")
        },
        |c| async move {
            let _ = c
                .storage()
                .from("docs")
                .create_signed_url(
                    "a.txt",
                    SignedUrlOptions {
                        expires_in: 600,
                        download: None,
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "storage.object.list",
        Expect {
            query: &[("prefix", "a")],
            ..e("GET", "/@portal/storage/buckets/docs/objects")
        },
        |c| async move {
            let _ = c.storage().from("docs").list(Some("a")).await;
        },
    )
    .await;
    check(
        "storage.object.remove",
        Expect {
            query: &[("path", "a.txt")],
            ..e("DELETE", "/@portal/storage/buckets/docs/objects")
        },
        |c| async move {
            let _ = c.storage().from("docs").remove("a.txt").await;
        },
    )
    .await;

    check(
        "ai.generate",
        Expect {
            body: Some(
                json!({"prompt":"hi","model":"llama-3-8b","maxTokens":10,"temperature":0.2}),
            ),
            ..e("POST", "/@portal/ai/generate")
        },
        |c| async move {
            let _ = c
                .ai()
                .generate(
                    "hi",
                    GenerateOptions {
                        model: Some("llama-3-8b"),
                        max_tokens: Some(10),
                        temperature: Some(0.2),
                        usage_type: None,
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "query.positional",
        Expect {
            body: Some(json!({"sql":"SELECT * FROM tasks WHERE done = $1","params":[false]})),
            ..e("POST", "/@portal/query")
        },
        |c| async move {
            let _ = c
                .query::<Value>("SELECT * FROM tasks WHERE done = $1", vec![json!(false)])
                .await;
        },
    )
    .await;
    check(
        "query.named",
        Expect {
            body: Some(json!({"sql":"SELECT * FROM tasks WHERE done = $1","params":[true]})),
            ..e("POST", "/@portal/query")
        },
        |c| async move {
            let mut m = Map::new();
            m.insert("done".into(), json!(true));
            let _ = c
                .query::<Value>("SELECT * FROM tasks WHERE done = :done", m)
                .await;
        },
    )
    .await;
    check(
        "data.textSearch",
        Expect {
            query: &[("textSearch", "body:hello world"), ("limit", "5")],
            ..e("GET", "/@portal/data/posts")
        },
        |c| async move {
            let _ = c
                .search::<Value>(
                    "posts",
                    k9o::SearchInput {
                        column: "body",
                        q: "hello world",
                        limit: Some(5),
                        ..Default::default()
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "ai.embed",
        Expect {
            body: Some(json!({"input":["a","b"]})),
            ..e("POST", "/@portal/ai/embeddings")
        },
        |c| async move {
            let _ = c
                .ai()
                .embed(EmbedInput {
                    model: None,
                    input: vec!["a", "b"].into(),
                })
                .await;
        },
    )
    .await;
    check(
        "compute.llms.list",
        e("GET", "/@portal/compute/llms"),
        |c| async move {
            let _ = c.compute().llms().list().await;
        },
    )
    .await;
    check(
        "compute.llms.read",
        e("GET", "/@portal/compute/llms/llama-3-8b"),
        |c| async move {
            let _ = c.compute().llms().read("llama-3-8b").await;
        },
    )
    .await;
    check(
        "compute.llms.invoke",
        Expect {
            body: Some(json!({"prompt":"hi","maxTokens":10,"temperature":0.2})),
            ..e("POST", "/@portal/compute/llms/llama-3-8b/invoke")
        },
        |c| async move {
            let input = LlmInvokeInput {
                prompt: "hi",
                params: LlmInvokeParams {
                    max_tokens: Some(10),
                    temperature: Some(0.2),
                    usage_type: None,
                },
            };
            let _ = c.compute().llms().invoke("llama-3-8b", input).await;
        },
    )
    .await;
    check(
        "compute.llms.invokeStream",
        Expect {
            body: Some(json!({"prompt":"hi"})),
            ..e("POST", "/@portal/compute/llms/llama-3-8b/invoke-stream")
        },
        |c| async move {
            let input = LlmInvokeInput {
                prompt: "hi",
                params: LlmInvokeParams::default(),
            };
            if let Ok(mut s) = c.compute().llms().invoke_stream("llama-3-8b", input).await {
                while s.next().await.is_some() {}
            }
        },
    )
    .await;

    check(
        "studio.workflows.list",
        Expect {
            query: &[("status", "active"), ("limit", "5"), ("cursor", "cur")],
            ..e("GET", "/@portal/workflows")
        },
        |c| async move {
            let _ = c
                .studio()
                .workflows()
                .list(Some("active"), Some(5), Some("cur"))
                .await;
        },
    )
    .await;
    let wf = json!({"name":"w","triggerType":"manual","triggerConfig":{},"actions":[{"index":0,"action_type":"branch","config":{}}]});
    check(
        "studio.workflows.create",
        Expect {
            body: Some(wf.clone()),
            ..e("POST", "/@portal/workflows")
        },
        |c| async move {
            let _ = c.studio().workflows().create(&wf).await;
        },
    )
    .await;
    check(
        "studio.workflows.read",
        e("GET", "/@portal/workflows/wfl_1"),
        |c| async move {
            let _ = c.studio().workflows().read("wfl_1").await;
        },
    )
    .await;
    check(
        "studio.workflows.update",
        Expect {
            body: Some(json!({"name":"w2"})),
            ..e("PATCH", "/@portal/workflows/wfl_1")
        },
        |c| async move {
            let _ = c
                .studio()
                .workflows()
                .update("wfl_1", &json!({"name":"w2"}))
                .await;
        },
    )
    .await;
    check(
        "studio.workflows.delete",
        e("DELETE", "/@portal/workflows/wfl_1"),
        |c| async move {
            let _ = c.studio().workflows().delete("wfl_1").await;
        },
    )
    .await;
    check(
        "studio.workflows.trigger",
        Expect {
            body: Some(json!({"triggerContext":{"k":"v"}})),
            ..e("POST", "/@portal/workflows/wfl_1/runs")
        },
        |c| async move {
            let _ = c
                .studio()
                .workflows()
                .trigger("wfl_1", &json!({"k":"v"}))
                .await;
        },
    )
    .await;
    check(
        "studio.workflows.runs.list",
        Expect {
            query: &[("limit", "5")],
            ..e("GET", "/@portal/workflows/wfl_1/runs")
        },
        |c| async move {
            let _ = c
                .studio()
                .workflows()
                .runs()
                .list("wfl_1", Some(5), None)
                .await;
        },
    )
    .await;
    check(
        "studio.workflows.runs.read",
        e("GET", "/@portal/workflows/wfl_1/runs/wfr_1"),
        |c| async move {
            let _ = c.studio().workflows().runs().read("wfl_1", "wfr_1").await;
        },
    )
    .await;
    check(
        "studio.workflows.runs.cancel",
        e("POST", "/@portal/workflows/wfl_1/runs/wfr_1/cancel"),
        |c| async move {
            let _ = c.studio().workflows().runs().cancel("wfl_1", "wfr_1").await;
        },
    )
    .await;
    check(
        "studio.dashboards.list",
        Expect {
            query: &[("status", "active")],
            ..e("GET", "/@portal/dashboards")
        },
        |c| async move {
            let _ = c.studio().dashboards().list(Some("active")).await;
        },
    )
    .await;
    check(
        "studio.dashboards.create",
        Expect {
            body: Some(json!({"name":"d"})),
            ..e("POST", "/@portal/dashboards")
        },
        |c| async move {
            let _ = c.studio().dashboards().create(&json!({"name":"d"})).await;
        },
    )
    .await;
    check(
        "studio.dashboards.read",
        e("GET", "/@portal/dashboards/dsh_1"),
        |c| async move {
            let _ = c.studio().dashboards().read("dsh_1").await;
        },
    )
    .await;
    check(
        "studio.dashboards.update",
        Expect {
            body: Some(json!({"name":"d2"})),
            ..e("PATCH", "/@portal/dashboards/dsh_1")
        },
        |c| async move {
            let _ = c
                .studio()
                .dashboards()
                .update("dsh_1", &json!({"name":"d2"}))
                .await;
        },
    )
    .await;
    check(
        "studio.dashboards.delete",
        e("DELETE", "/@portal/dashboards/dsh_1"),
        |c| async move {
            let _ = c.studio().dashboards().delete("dsh_1").await;
        },
    )
    .await;
    check(
        "studio.dashboards.pin",
        e("POST", "/@portal/dashboards/dsh_1/pin"),
        |c| async move {
            let _ = c.studio().dashboards().pin("dsh_1").await;
        },
    )
    .await;
    check(
        "studio.dashboards.unpin",
        e("POST", "/@portal/dashboards/dsh_1/unpin"),
        |c| async move {
            let _ = c.studio().dashboards().unpin("dsh_1").await;
        },
    )
    .await;
}

#[tokio::test]
async fn contract_kibo_gate_realtime_data() {
    let p = "/@portal/vault/secrets/db%2Fpw";
    check(
        "kibo.vault.secrets.list",
        Expect {
            query: &[("prefix", "db/")],
            ..e("GET", "/@portal/vault/secrets")
        },
        |c| async move {
            let _ = c
                .kibo()
                .vault()
                .secrets()
                .list(ListSecretsInput {
                    prefix: Some("db/"),
                    ..Default::default()
                })
                .await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.get",
        Expect {
            query: &[("prefix", "db/pw"), ("limit", "100")],
            ..e("GET", "/@portal/vault/secrets")
        },
        |c| async move {
            let _ = c.kibo().vault().secrets().get("db/pw", "").await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.create",
        Expect {
            query: &[("region", "de01")],
            body: Some(json!({"value":"eA=="})),
            ..e("POST", p)
        },
        |c| async move {
            let input = CreateSecretInput {
                name: "db/pw",
                value: "x",
                region: "de01",
                visibility: None,
                metadata: None,
            };
            let _ = c.kibo().vault().secrets().create(input).await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.create.noRegion",
        Expect {
            body: Some(json!({"value":"eA=="})),
            ..e("POST", p)
        },
        |c| async move {
            let input = CreateSecretInput {
                name: "db/pw",
                value: "x",
                region: "",
                visibility: None,
                metadata: None,
            };
            let _ = c.kibo().vault().secrets().create(input).await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.revealChallenge",
        Expect {
            query: &[("op", "reveal-challenge")],
            ..e("POST", p)
        },
        |c| async move {
            let _ = c
                .kibo()
                .vault()
                .secrets()
                .reveal_challenge("db/pw", "")
                .await;
        },
    )
    .await;
    check("kibo.vault.secrets.reveal", e("GET", p), |c| async move {
        let _ = c.kibo().vault().secrets().reveal("db/pw", "", "chal").await;
    })
    .await;
    check(
        "kibo.vault.secrets.update",
        Expect {
            query: &[("region", "de01")],
            body: Some(json!({"value":"eQ=="})),
            ..e("PUT", p)
        },
        |c| async move {
            let _ = c
                .kibo()
                .vault()
                .secrets()
                .update(
                    "db/pw",
                    UpdateSecretInput {
                        region: "de01",
                        value: "y",
                        metadata: None,
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.rotate",
        Expect {
            query: &[("op", "rotate")],
            ..e("POST", p)
        },
        |c| async move {
            let _ = c.kibo().vault().secrets().rotate("db/pw", "").await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.move",
        Expect {
            query: &[("op", "move")],
            body: Some(json!({"newPath":"db/pw2"})),
            ..e("POST", p)
        },
        |c| async move {
            let _ = c
                .kibo()
                .vault()
                .secrets()
                .r#move("db/pw", "", "db/pw2")
                .await;
        },
    )
    .await;
    check(
        "kibo.vault.secrets.delete",
        e("DELETE", p),
        |c| async move {
            let _ = c.kibo().vault().secrets().delete("db/pw", "").await;
        },
    )
    .await;

    check(
        "gate.signUp",
        Expect {
            body: Some(json!({"email":"a@b.co","password":"pw","displayName":"A"})),
            ..e("POST", "/@portal/gate/signup")
        },
        |c| async move {
            let _ = c
                .gate()
                .sign_up(SignUpInput {
                    email: "a@b.co",
                    password: "pw",
                    display_name: Some("A"),
                })
                .await;
        },
    )
    .await;
    check(
        "gate.signIn",
        Expect {
            body: Some(json!({"email":"a@b.co","password":"pw"})),
            door: Some("a@b.co"),
            ..e("POST", "/@portal/gate/signin")
        },
        |c| async move {
            let _ = c
                .gate()
                .sign_in(SignInInput {
                    email: "a@b.co",
                    password: "pw",
                })
                .await;
        },
    )
    .await;
    check(
        "gate.signOut",
        Expect {
            body: Some(json!({"refreshToken":"rt"})),
            ..e("POST", "/@portal/gate/signout")
        },
        |c| async move {
            let _ = c
                .gate()
                .sign_out(SignOutInput {
                    refresh_token: "rt",
                })
                .await;
        },
    )
    .await;
    check(
        "gate.whoami",
        e("GET", "/@portal/gate/whoami"),
        |c| async move {
            let _ = c.gate().whoami().await;
        },
    )
    .await;
    check(
        "gate.revokeAllSessions",
        e("POST", "/@portal/auth/sessions/revoke-all"),
        |c| async move {
            let _ = c.gate().revoke_all_sessions().await;
        },
    )
    .await;
    check(
        "gate.refreshToken",
        Expect {
            body: Some(json!({"refreshToken":"rt"})),
            ..e("POST", "/@portal/gate/refresh")
        },
        |c| async move {
            let _ = c
                .gate()
                .refresh_token(RefreshTokenInput {
                    refresh_token: "rt",
                })
                .await;
        },
    )
    .await;

    check(
        "realtime.subscribe",
        Expect {
            query: &[("topic", "collections.orders.*")],
            ..e("GET", "/@portal/realtime/subscribe")
        },
        |c| async move {
            // The feed reconnects by itself, so read once and stop.
            if let Ok(mut s) = c.realtime().subscribe("collections.orders.*", None).await {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(200), s.next()).await;
                s.close();
            }
        },
    )
    .await;

    // Streams (user data streams).
    check(
        "streams.create",
        Expect {
            body: Some(
                json!({"displayName":"Orders","maxBytes":1073741824u64,"retentionSeconds":86400}),
            ),
            ..e("POST", "/@portal/data-streams/user")
        },
        |c| async move {
            let _ = c.streams().create("Orders", 1073741824, Some(86400)).await;
        },
    )
    .await;
    check(
        "streams.list",
        e("GET", "/@portal/data-streams/user"),
        |c| async move {
            let _ = c.streams().list().await;
        },
    )
    .await;
    check(
        "streams.get",
        e("GET", "/@portal/data-streams/user/orders"),
        |c| async move {
            let _ = c.streams().get("orders").await;
        },
    )
    .await;
    check(
        "streams.delete",
        e("DELETE", "/@portal/data-streams/user/orders"),
        |c| async move {
            let _ = c.streams().delete("orders").await;
        },
    )
    .await;
    check(
        "streams.publish",
        Expect {
            body: Some(json!({"data":{"id":1},"subject":"eu.new"})),
            ..e("POST", "/@portal/data-streams/user/orders/publish")
        },
        |c| async move {
            let _ = c
                .streams()
                .publish("orders", &json!({"id": 1}), Some("eu.new"))
                .await;
        },
    )
    .await;
    check(
        "streams.replay",
        Expect {
            body: Some(json!({"fromSequence":10,"untilSequence":50,"limit":25})),
            ..e("POST", "/@portal/data-streams/user/orders/replay")
        },
        |c| async move {
            let _ = c
                .streams()
                .replay("orders", Some(10), Some(50), Some(25))
                .await;
        },
    )
    .await;
    check(
        "streams.subscribe",
        Expect {
            query: &[("fromSequence", "5")],
            ..e("GET", "/@portal/data-streams/user/orders/subscribe")
        },
        |c| async move {
            // The first connection is opened by subscribe; close before it reconnects.
            if let Ok(mut s) = c.streams().subscribe("orders", Some("5")).await {
                s.close();
            }
        },
    )
    .await;

    // Approvals, approval policies, schema collections (the OpenAPI declares no bodies for these).
    check(
        "approvals.list",
        Expect {
            query: &[("status", "pending_approval"), ("limit", "20")],
            ..e("GET", "/@portal/pending-approvals")
        },
        |c| async move {
            let _ = c
                .approvals()
                .list(ListApprovalsOptions {
                    status: Some("pending_approval"),
                    limit: Some(20),
                    ..Default::default()
                })
                .await;
        },
    )
    .await;
    check(
        "approvals.get",
        e("GET", "/@portal/pending-approvals/apr_1"),
        |c| async move {
            let _ = c.approvals().get("apr_1").await;
        },
    )
    .await;
    check(
        "approvals.approve",
        Expect {
            body: Some(json!({"reason":"ok"})),
            contract: Some(json!([[], ["reason"]])),
            ..e("POST", "/@portal/pending-approvals/apr_1/approve")
        },
        |c| async move {
            let _ = c
                .approvals()
                .approve("apr_1", ApproveInput { reason: Some("ok") })
                .await;
        },
    )
    .await;
    check(
        "approvals.reject",
        Expect {
            body: Some(json!({"reason":"duplicate"})),
            contract: Some(json!([["reason"], ["reason"]])),
            ..e("POST", "/@portal/pending-approvals/apr_1/reject")
        },
        |c| async move {
            let _ = c
                .approvals()
                .reject(
                    "apr_1",
                    RejectInput {
                        reason: "duplicate",
                    },
                )
                .await;
        },
    )
    .await;
    check(
        "approvals.bulk",
        Expect {
            body: Some(
                json!({"action":"reject","criteria":{"eq":["change.table","orders"]},"reason":"stale"}),
            ),
            contract: Some(json!([["action", "criteria"], ["action", "criteria", "reason"]])),
            ..e("POST", "/@portal/pending-approvals/bulk")
        },
        |c| async move {
            let criteria = json!({"eq": ["change.table", "orders"]});
            let _ = c
                .approvals()
                .bulk(BulkApprovalsInput {
                    action: "reject",
                    criteria: &criteria,
                    reason: Some("stale"),
                })
                .await;
        },
    )
    .await;
    check(
        "approvalPolicies.list",
        e("GET", "/@portal/approval-policies"),
        |c| async move {
            let _ = c.approval_policies().list().await;
        },
    )
    .await;
    check(
        "approvalPolicies.create",
        Expect {
            body: Some(
                json!({"collection":"orders","operation":"update","action":"queue","predicate":{"gt":["change.row.amount",1000]}}),
            ),
            contract: Some(json!([
                ["collection", "operation", "action", "predicate"],
                ["collection", "operation", "action", "predicate"]
            ])),
            ..e("POST", "/@portal/approval-policies")
        },
        |c| async move {
            let predicate = json!({"gt": ["change.row.amount", 1000]});
            let _ = c
                .approval_policies()
                .create(CreateApprovalPolicyInput {
                    collection: "orders",
                    operation: "update",
                    action: "queue",
                    predicate: &predicate,
                })
                .await;
        },
    )
    .await;
    check(
        "approvalPolicies.delete",
        e("DELETE", "/@portal/approval-policies/apo_1"),
        |c| async move {
            let _ = c.approval_policies().delete("apo_1").await;
        },
    )
    .await;
    check(
        "schema.collections.list",
        Expect {
            query: &[("limit", "10"), ("cursor", "abc")],
            ..e("GET", "/@portal/schema/collections")
        },
        |c| async move {
            let _ = c
                .schema()
                .collections()
                .list(ListCollectionsOptions {
                    limit: Some(10),
                    cursor: Some("abc"),
                })
                .await;
        },
    )
    .await;

    // Data plane: the OpenAPI declares no body for these; the record fields are the collection's columns.
    check(
        "data.select",
        Expect {
            query: &[("status", "eq.published"), ("limit", "5")],
            ..e("GET", "/@portal/data/posts")
        },
        |c| async move {
            let _ = c
                .from("posts")
                .select()
                .eq("status", "published")
                .limit(5)
                .execute::<Value>()
                .await;
        },
    )
    .await;
    check(
        "data.insert",
        Expect {
            body: Some(json!({"title":"x"})),
            contract: Some(json!([[], null])),
            ..e("POST", "/@portal/data/posts")
        },
        |c| async move {
            let _ = c
                .from("posts")
                .insert::<Value, _>(&json!({"title":"x"}))
                .await;
        },
    )
    .await;
    check(
        "data.update",
        Expect {
            body: Some(json!({"title":"y"})),
            contract: Some(json!([[], null])),
            ..e("PATCH", "/@portal/data/posts/rec_1")
        },
        |c| async move {
            let _ = c
                .from("posts")
                .update(json!({"title":"y"}))
                .eq("id", "rec_1")
                .execute::<Value>()
                .await;
        },
    )
    .await;
    check(
        "data.delete",
        e("DELETE", "/@portal/data/posts/rec_1"),
        |c| async move {
            let _ = c
                .from("posts")
                .delete()
                .eq("id", "rec_1")
                .execute::<Value>()
                .await;
        },
    )
    .await;
}
