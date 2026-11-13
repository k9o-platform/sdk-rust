use k9o::{
    org_path, platform_path, project_path, resolve_base_url, validate_base_url, validate_slug,
};

#[test]
fn test_validate_base_url_accepts() {
    assert_eq!(
        validate_base_url("https://api.k9o.io").unwrap(),
        "https://api.k9o.io"
    );
    assert_eq!(
        validate_base_url("https://acme.api.k9o.io").unwrap(),
        "https://acme.api.k9o.io"
    );
    assert_eq!(
        validate_base_url("http://localhost:8080").unwrap(),
        "http://localhost:8080"
    );
    assert_eq!(
        validate_base_url("https://api.k9o.io/").unwrap(),
        "https://api.k9o.io"
    );
}

#[test]
fn test_validate_base_url_rejects_path() {
    let e = validate_base_url("https://api.k9o.io/v2").unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
}

#[test]
fn test_validate_base_url_rejects_websocket() {
    let e = validate_base_url("wss://api.k9o.io").unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
}

#[test]
fn test_validate_base_url_rejects_no_scheme() {
    let e = validate_base_url("api.k9o.io").unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
}

#[test]
fn test_validate_slug_accepts() {
    assert!(validate_slug("acme", "org").is_ok());
    assert!(validate_slug("member-portal", "project").is_ok());
    assert!(validate_slug("a1", "org").is_ok());
    assert!(validate_slug("a-1-z", "project").is_ok());
}

#[test]
fn test_validate_slug_rejects() {
    let bad = ["Acme", "1acme", "-acme", "acme-", "a", ""];
    for s in bad {
        let e = validate_slug(s, "test");
        assert!(e.is_err(), "expected reject for {:?}", s);
        assert_eq!(e.unwrap_err().code, "K9O_INVALID_FIELD_VALUE");
    }
}

#[test]
fn test_project_path() {
    assert_eq!(
        project_path("member-portal", "data/posts"),
        "/@member-portal/data/posts"
    );
    assert_eq!(project_path("acme", "/project"), "/@acme/project");
}

#[test]
fn test_org_path() {
    assert_eq!(org_path("projects"), "/projects");
    assert_eq!(org_path("/projects"), "/projects");
}

#[test]
fn test_platform_path() {
    assert_eq!(platform_path("discovery/projects"), "/discovery/projects");
}

// resolve_base_url — mirrors TS resolveBaseUrl.

#[test]
fn test_resolve_base_url_default_apex_no_org() {
    // Omitted base_url, no org → platform apex.
    assert_eq!(resolve_base_url(None, None).unwrap(), "https://api.k9o.io");
}

#[test]
fn test_resolve_base_url_derives_subdomain_from_omitted_apex() {
    // Omitted base_url (defaults to apex) + org_slug → derive org subdomain.
    assert_eq!(
        resolve_base_url(None, Some("acme")).unwrap(),
        "https://acme.api.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_derives_subdomain_from_explicit_apex() {
    // Explicit bare apex + org_slug → derive org subdomain (scheme preserved).
    assert_eq!(
        resolve_base_url(Some("https://api.k9o.io"), Some("acme")).unwrap(),
        "https://acme.api.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_explicit_subdomain_unchanged() {
    // Explicit <orgSlug> subdomain passes through untouched, even with org_slug set.
    assert_eq!(
        resolve_base_url(Some("https://acme.api.k9o.io"), Some("acme")).unwrap(),
        "https://acme.api.k9o.io"
    );
    assert_eq!(
        resolve_base_url(Some("https://other.api.k9o.io"), Some("acme")).unwrap(),
        "https://other.api.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_self_hosted_unchanged() {
    // Self-hosted / Sovereign host passes through, org_slug ignored for host rewrite.
    assert_eq!(
        resolve_base_url(Some("https://platform.acme.internal"), Some("acme")).unwrap(),
        "https://platform.acme.internal"
    );
}

#[test]
fn test_resolve_base_url_dev_host_unchanged() {
    // Dev host passes through (scheme + port preserved).
    assert_eq!(
        resolve_base_url(Some("http://localhost:8080"), Some("acme")).unwrap(),
        "http://localhost:8080"
    );
}

#[test]
fn test_resolve_base_url_apex_no_org_unchanged() {
    // Bare apex without org_slug → apex unchanged (no derivation).
    assert_eq!(
        resolve_base_url(Some("https://api.k9o.io"), None).unwrap(),
        "https://api.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_staging_apex_derives_subdomain() {
    // Bare staging apex + org_slug → derive org subdomain (the cross-pool fix).
    assert_eq!(
        resolve_base_url(Some("https://api-staging.k9o.io"), Some("acme")).unwrap(),
        "https://acme.api-staging.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_staging_explicit_subdomain_unchanged() {
    // Explicit staging subdomain passes through untouched.
    assert_eq!(
        resolve_base_url(Some("https://acme.api-staging.k9o.io"), Some("acme")).unwrap(),
        "https://acme.api-staging.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_staging_apex_no_org_unchanged() {
    // Bare staging apex without org_slug → unchanged.
    assert_eq!(
        resolve_base_url(Some("https://api-staging.k9o.io"), None).unwrap(),
        "https://api-staging.k9o.io"
    );
}

#[test]
fn test_resolve_base_url_staging_invalid_slug_rejected() {
    // Slug validated on the staging apex derive path too → INVALID_FIELD_VALUE.
    let e = resolve_base_url(Some("https://api-staging.k9o.io"), Some("Bad_Slug")).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
}

#[test]
fn test_resolve_base_url_invalid_slug_rejected() {
    // Derivation validates the slug first → INVALID_FIELD_VALUE.
    let e = resolve_base_url(None, Some("Acme")).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
}

#[test]
fn test_resolve_base_url_keeps_base_url_validation() {
    // Existing baseUrl validation still applies (path-bearing rejected).
    let e = resolve_base_url(Some("https://api.k9o.io/v2"), None).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
    let e = resolve_base_url(Some("wss://api.k9o.io"), None).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_BASE_URL");
}

#[test]
fn parse_api_host_reads_org_region_and_apex_from_any_apex() {
    use k9o::{parse_api_host, ApiHost};
    let h = |org: Option<&str>, family: &str, region: Option<&str>, apex: &str| ApiHost {
        org_slug: org.map(str::to_string),
        family: family.to_string(),
        region: region.map(str::to_string),
        apex: apex.to_string(),
    };
    assert_eq!(
        parse_api_host("acme.api.k9o.io"),
        Some(h(Some("acme"), "api", None, "k9o.io"))
    );
    assert_eq!(
        parse_api_host("acme.api-de01.corp.internal"),
        Some(h(Some("acme"), "api", Some("de01"), "corp.internal"))
    );
    assert_eq!(
        parse_api_host("acme.api-staging-de01.example.test"),
        Some(h(Some("acme"), "api-staging", Some("de01"), "example.test"))
    );
    assert_eq!(
        parse_api_host("api.corp.internal"),
        Some(h(None, "api", None, "corp.internal"))
    );
    for host in [
        "localhost",
        "127.0.0.1",
        "gateway.corp.internal",
        "acme.api-germany.k9o.io",
    ] {
        assert_eq!(parse_api_host(host), None, "{host}");
    }
}

#[test]
fn resolve_base_url_derives_the_org_host_on_any_apex() {
    use k9o::resolve_base_url;
    assert_eq!(
        resolve_base_url(Some("https://api.corp.internal"), Some("acme")).unwrap(),
        "https://acme.api.corp.internal"
    );
    assert_eq!(
        resolve_base_url(Some("https://api.corp.internal:8443"), Some("acme")).unwrap(),
        "https://acme.api.corp.internal:8443"
    );
    // Already an org host, a regional host, or an unrelated host: unchanged.
    for base in [
        "https://acme.api.corp.internal",
        "https://acme.api-de01.corp.internal",
        "https://gateway.corp.internal",
        "http://localhost:8080",
    ] {
        assert_eq!(resolve_base_url(Some(base), Some("acme")).unwrap(), base);
    }
}
