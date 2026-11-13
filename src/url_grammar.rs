//! URL grammar helpers.

use crate::kerror::KError;
use url::Url;

/// Validate + normalise a baseUrl.
/// Returns `Ok(normalised)` or `Err(KError(K9O_INVALID_BASE_URL))`.
pub fn validate_base_url(base_url: &str) -> Result<String, KError> {
    let parsed = Url::parse(base_url).map_err(|e| {
        KError::new(
            "K9O_INVALID_BASE_URL",
            format!("base_url {:?} is not a valid URL: {}", base_url, e),
        )
        .with_context("base_url", base_url)
    })?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(KError::new(
            "K9O_INVALID_BASE_URL",
            format!(
                "base_url {:?} uses unsupported scheme {:?}; expected http or https",
                base_url, scheme
            ),
        )
        .with_context("base_url", base_url)
        .with_context("scheme", scheme));
    }
    if parsed.host_str().is_none() {
        return Err(KError::new(
            "K9O_INVALID_BASE_URL",
            format!("base_url {:?} missing host", base_url),
        ));
    }
    if parsed.path() != "" && parsed.path() != "/" {
        return Err(KError::new(
            "K9O_INVALID_BASE_URL",
            format!(
                "base_url {:?} must not contain a path segment; got {:?}",
                base_url,
                parsed.path()
            ),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(KError::new(
            "K9O_INVALID_BASE_URL",
            format!("base_url {:?} must not contain query or fragment", base_url),
        ));
    }

    let host = parsed.host_str().unwrap();
    let normalised = match parsed.port() {
        Some(port) => format!("{}://{}:{}", scheme, host, port),
        None => format!("{}://{}", scheme, host),
    };
    Ok(normalised)
}

/// The convenience default when no `base_url` is given. A default value only:
/// it is never used to parse or match a host.
pub const DEFAULT_BASE_URL: &str = "https://api.k9o.io";

/// The structure of a gateway API host, read from the host itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiHost {
    /// First label of an org host (`acme` in `acme.api.corp.internal`);
    /// `None` for a platform host (`api.corp.internal`).
    pub org_slug: Option<String>,
    /// `api` or `api-staging`: the label that marks a gateway host.
    pub family: String,
    /// 4-character region code when the host is regional (`api-de01`), else
    /// `None` (region-free).
    pub region: Option<String>,
    /// Everything after the gateway label (`corp.internal`, `k9o.io`): the
    /// apex, never matched against a list.
    pub apex: String,
}

/// Match `api` / `api-staging`, optionally followed by `-<rgn>` (`[a-z]{2}\d{2}`).
/// Returns `(family, region)`.
fn parse_gateway_label(label: &str) -> Option<(&'static str, Option<String>)> {
    let (family, rest) = if let Some(r) = label.strip_prefix("api-staging") {
        ("api-staging", r)
    } else if let Some(r) = label.strip_prefix("api") {
        ("api", r)
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some((family, None));
    }
    let region = rest.strip_prefix('-')?;
    let b = region.as_bytes();
    let ok = b.len() == 4
        && b[0].is_ascii_lowercase()
        && b[1].is_ascii_lowercase()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit();
    ok.then(|| (family, Some(region.to_string())))
}

fn slug_label_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && !s.ends_with('-')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Read the org slug and region out of a gateway host without knowing its
/// apex. The gateway label is `api` or `api-staging`, optionally followed by
/// `-<rgn>`; the label before it is the org slug and whatever follows it is
/// the apex. Returns `None` for any other shape (localhost, an IP, a
/// single-label host, a host with no gateway label); callers use such a host
/// exactly as given.
pub fn parse_api_host(host: &str) -> Option<ApiHost> {
    let lower = host.to_ascii_lowercase();
    let labels: Vec<&str> = lower.split('.').collect();
    if labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    let mk = |gateway: &str, org: Option<&str>, rest: &[&str]| -> Option<ApiHost> {
        let (family, region) = parse_gateway_label(gateway)?;
        if rest.is_empty() {
            return None;
        }
        Some(ApiHost {
            org_slug: org.map(str::to_string),
            family: family.to_string(),
            region,
            apex: rest.join("."),
        })
    };
    // Org host first: `<org>.<api[-rgn]>.<apex>`.
    if labels.len() >= 3 && slug_label_ok(labels[0]) && parse_gateway_label(labels[1]).is_some() {
        return mk(labels[1], Some(labels[0]), &labels[2..]);
    }
    // Platform host: `api.<apex>`.
    if labels.len() >= 2 && parse_gateway_label(labels[0]).is_some() {
        return mk(labels[0], None, &labels[1..]);
    }
    None
}

/// Resolve the effective baseUrl. `base_url` is OPTIONAL: omitted, it defaults
/// to [`DEFAULT_BASE_URL`]. When the (validated) host is a region-free
/// platform host (`api.<apex>` or `api-staging.<apex>`, any apex) and
/// `org_slug` is supplied, the org host `<org_slug>.<host>` is composed. Any
/// other host (an org host already, an internal or air-gapped host, localhost)
/// passes through unchanged; `org_slug` is then only sent as a header.
///
/// Mirrors `resolveBaseUrl(baseUrl?, orgSlug?)` in `packages/core/src/url-grammar.ts`.
pub fn resolve_base_url(base_url: Option<&str>, org_slug: Option<&str>) -> Result<String, KError> {
    let validated = validate_base_url(base_url.unwrap_or(DEFAULT_BASE_URL))?;
    // Re-parse the validated form to inspect host + scheme.
    let parsed = Url::parse(&validated).map_err(|e| {
        KError::new(
            "K9O_INVALID_BASE_URL",
            format!("base_url {:?} is not a valid URL: {}", validated, e),
        )
        .with_context("base_url", validated.as_str())
    })?;
    if let (Some(host), Some(slug)) = (parsed.host_str(), org_slug) {
        if let Some(h) = parse_api_host(host) {
            if h.org_slug.is_none() && h.region.is_none() {
                validate_slug(slug, "org")?;
                let authority = match parsed.port() {
                    Some(p) => format!("{}:{}", host, p),
                    None => host.to_string(),
                };
                return Ok(format!("{}://{}.{}", parsed.scheme(), slug, authority));
            }
        }
    }
    Ok(validated)
}

/// Validate an org/project slug.
pub fn validate_slug(slug: &str, kind: &str) -> Result<(), KError> {
    let valid = slug.len() >= 2
        && slug.len() <= 63
        && slug
            .chars()
            .next()
            .map_or(false, |c| c.is_ascii_lowercase())
        && !slug.ends_with('-')
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        return Err(KError::new(
            "K9O_INVALID_FIELD_VALUE",
            format!(
                "{}_slug {:?} does not match required format ^[a-z][a-z0-9-]{{1,62}}$ (must not end with hyphen)",
                kind, slug
            ),
        )
        .with_context("slug", slug)
        .with_context("kind", kind));
    }
    Ok(())
}

/// Member-plane path: `/@<slug>/<sub-resource>`.
pub fn project_path(project_slug: &str, sub_resource: &str) -> String {
    let cleaned = sub_resource.strip_prefix('/').unwrap_or(sub_resource);
    format!("/@{}/{}", project_slug, cleaned)
}

/// Org-plane path: `/<resource>`.
pub fn org_path(resource: &str) -> String {
    format!("/{}", resource.strip_prefix('/').unwrap_or(resource))
}

/// Platform-plane path: `/<resource>`.
pub fn platform_path(resource: &str) -> String {
    format!("/{}", resource.strip_prefix('/').unwrap_or(resource))
}

/// Percent-encode one path segment (the equivalent of JavaScript's
/// `encodeURIComponent`), so an id containing `/` or `?` stays one segment.
pub(crate) fn enc(segment: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const KEEP: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    utf8_percent_encode(segment, KEEP).to_string()
}
