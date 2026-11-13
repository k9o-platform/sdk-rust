//! `KiboCore` against the real Kibo core, with placeholder credentials and no real cloud.
//!
//! The Vault cases run against a local stub on loopback and always run. The AWS, GCP and Azure
//! cases reach the provider's public host, where placeholder credentials are refused; they run
//! only when `K9O_KIBO_CORE_NETWORK_TESTS=1` is set.

use k9o::{
    AppCore, AppCredential, AppKeyRef, AppOpenInput, AppSealInput, FederationInput, FederationSpec,
    KmsProvider, Posture, ProviderConfig,
};
use k9o_kibo_core::KiboCore;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn cred() -> AppCredential {
    AppCredential { secret: "placeholder-credential".into(), expires_at_unix_secs: 4_102_444_800 }
}

fn seal(provider: KmsProvider, key: &str, cfg: ProviderConfig) -> AppSealInput {
    AppSealInput {
        provider,
        posture: Posture::Byok,
        kms_cred: cred(),
        key_ref: AppKeyRef { version: 1, external_ref: key.into() },
        provider_config: cfg,
        project_id: "proj_placeholder".into(),
        crypt: "placeholder".into(),
        context: Some("ctx".into()),
        plaintext: b"placeholder".to_vec(),
        expected_key_refs: vec![key.into()],
    }
}

/// A loopback server that answers every request 403 and records the request line.
async fn stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { continue };
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.lock().unwrap().push(head.lines().next().unwrap_or("").to_string());
                let body = r#"{"errors":["permission denied"]}"#;
                let r = format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(r.as_bytes()).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (addr, seen)
}

fn refused_upstream(e: &str) {
    assert!(e.starts_with("UPSTREAM_KMS_ERROR: "), "not an upstream refusal: {e}");
    assert!(!e.contains("placeholder-credential"), "the credential leaked: {e}");
}

#[test]
fn capabilities_list_the_pin() {
    assert!(KiboCore::new().capabilities().iter().any(|c| c == "expected_key_refs"));
}

#[tokio::test]
async fn vault_seal_reaches_the_provider_and_is_refused_there() {
    let (addr, seen) = stub().await;
    let cfg = ProviderConfig { aws_region: None, vault_address: Some(addr) };
    let e = KiboCore::new()
        .seal(seal(KmsProvider::HashicorpVault, "transit/k:1", cfg))
        .await
        .unwrap_err();
    println!("vault seal: {e}");
    refused_upstream(&e);
    assert!(!seen.lock().unwrap().is_empty(), "the stub saw no request");
}

#[tokio::test]
async fn vault_login_uses_the_jwt_auth_mount() {
    let (addr, seen) = stub().await;
    let e = KiboCore::new()
        .acquire(FederationInput {
            spec: FederationSpec::Vault {
                vault_addr: addr,
                jwt_auth_mount: "my-jwt".into(),
                role: "app".into(),
            },
            identity_token: "placeholder-token".into(),
            now_unix_secs: 1_700_000_000,
        })
        .await
        .unwrap_err();
    println!("vault login: {e}");
    assert!(!e.contains("placeholder-token"), "the token leaked: {e}");
    let lines = seen.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.starts_with("POST /v1/auth/my-jwt/login ")),
        "the login did not go to auth/my-jwt: {lines:?}"
    );
}

#[tokio::test]
async fn a_malformed_container_is_the_cores_own_error_not_a_conversion_error() {
    let e = KiboCore::new()
        .open(AppOpenInput {
            kms_cred: cred(),
            provider_config: ProviderConfig::default(),
            project_id: "proj_placeholder".into(),
            crypt: "placeholder".into(),
            context: None,
            container: vec![1, 2, 3],
            expected_key_refs: vec!["transit/k:1".into()],
        })
        .await
        .unwrap_err();
    println!("open: {e}");
    let code = e.split_once(": ").map(|(c, _)| c).unwrap_or("");
    assert!(!code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "{e}");
}

fn network_tests() -> bool {
    std::env::var("K9O_KIBO_CORE_NETWORK_TESTS").as_deref() == Ok("1")
}

#[tokio::test]
async fn aws_gcp_azure_seal_is_refused_by_the_provider() {
    if !network_tests() {
        eprintln!("skipped: set K9O_KIBO_CORE_NETWORK_TESTS=1 to reach the public provider hosts");
        return;
    }
    let cases = [
        (
            "aws",
            KmsProvider::AwsKms,
            "arn:aws:kms:eu-central-1:111122223333:key/00000000-0000-0000-0000-000000000000",
            ProviderConfig { aws_region: Some("eu-central-1".into()), vault_address: None },
        ),
        (
            "gcp",
            KmsProvider::GcpKms,
            "projects/placeholder/locations/global/keyRings/r/cryptoKeys/k",
            ProviderConfig::default(),
        ),
        (
            "azure",
            KmsProvider::AzureKv,
            "https://placeholder-vault.vault.azure.net/keys/k/00000000000000000000000000000000",
            ProviderConfig::default(),
        ),
    ];
    for (name, p, key, cfg) in cases {
        let mut input = seal(p, key, cfg);
        if name == "aws" {
            // AWS wants an STS-shaped credential object; these are not real keys.
            input.kms_cred.secret =
                r#"{"AccessKeyId":"AKIAPLACEHOLDER000","SecretAccessKey":"placeholder","SessionToken":"placeholder"}"#.into();
        }
        let e = KiboCore::new().seal(input).await.unwrap_err();
        println!("{name} seal: {e}");
        refused_upstream(&e);
    }
}
