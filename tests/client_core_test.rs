//! The Kibo client core for BYOK and HYOK projects, over a fake Kibo core.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use k9o::{
    AppCore, AppCredential, AppKeyRef, AppOpenInput, AppSealInput, ClientCore, CoreOptions,
    CryptoBatch, CryptoFederatedToken, FederatedTokenFn, FederationInput, FederationSpec,
    KeyRefProvider, KiboClientCore, KiboClientCoreConfig, PlainItem, Posture, SealedItem,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "arn:aws:kms:eu-central-1:111122223333:key/mine";
type Log = Arc<Mutex<Vec<String>>>;

fn log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}
fn entries(l: &Log) -> Vec<String> {
    l.lock().unwrap().clone()
}

struct Fake {
    log: Log,
    caps: Vec<String>,
    missing: Vec<&'static str>,
    stall: bool,
    seal_err: Option<String>,
}

fn fake(log: &Log) -> Fake {
    Fake {
        log: log.clone(),
        caps: vec!["expected_key_refs".into()],
        missing: vec![],
        stall: false,
        seal_err: None,
    }
}

#[async_trait]
impl AppCore for Fake {
    fn capabilities(&self) -> Vec<String> {
        self.caps.clone()
    }
    fn missing_exports(&self) -> Vec<&'static str> {
        self.missing.clone()
    }
    async fn seal(&self, i: AppSealInput) -> Result<Vec<u8>, String> {
        self.log.lock().unwrap().push(format!(
            "seal:{}:{}:{}",
            i.crypt,
            i.project_id,
            i.expected_key_refs.join(",")
        ));
        if let Some(e) = &self.seal_err {
            return Err(e.clone());
        }
        let mut c = vec![1, 2, 3];
        c.extend(i.plaintext);
        Ok(c)
    }
    async fn open(&self, i: AppOpenInput) -> Result<Vec<u8>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("open:{}:{}", i.crypt, i.expected_key_refs.join(",")));
        if i.container.first() != Some(&1) {
            return Err("K9O_CRYPTO_UNEXPECTED_KEY: no".into());
        }
        Ok(i.container[3..].to_vec())
    }
    async fn acquire(&self, i: FederationInput) -> Result<AppCredential, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("exchange:{}", i.identity_token));
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(AppCredential {
            secret: "cred-1".into(),
            expires_at_unix_secs: 10_000,
        })
    }
}

fn tok(provider: Option<&str>, project: Option<&str>) -> CryptoFederatedToken {
    CryptoFederatedToken {
        token: "jwt-secret".into(),
        expires_at: "x".into(),
        issuer: "i".into(),
        subject: "proj_1:crypt:c:op:encrypt".into(),
        audience: "a".into(),
        kid: "k".into(),
        provider: provider.map(Into::into),
        project_id: project.map(Into::into),
    }
}

fn token_fn(log: &Log, t: CryptoFederatedToken) -> FederatedTokenFn {
    let log = log.clone();
    Arc::new(move |req| {
        log.lock()
            .unwrap()
            .push(format!("token:{}:{}", req.crypt, req.op.as_str()));
        let t = t.clone();
        Box::pin(async move { Ok(t) })
    })
}

fn cfg_with(log: &Log, core: Fake, t: CryptoFederatedToken) -> KiboClientCoreConfig {
    let mut c = KiboClientCoreConfig::new(
        KeyRefProvider::Aws,
        Posture::Byok,
        AppKeyRef {
            version: 1,
            external_ref: KEY.into(),
        },
        FederationSpec::Aws {
            role_arn: "arn:aws:iam::1:role/r".into(),
            session_name: "s".into(),
            region: "eu-central-1".into(),
            key_arn: KEY.into(),
            duration_secs: 900,
        },
        vec![KEY.into()],
        token_fn(log, t),
        Arc::new(core),
    );
    c.project_id = Some("proj_1".into());
    c.now = Arc::new(|| 1000);
    c
}

fn cfg(log: &Log) -> KiboClientCoreConfig {
    cfg_with(log, fake(log), tok(Some("aws"), Some("proj_1")))
}

fn built(c: KiboClientCoreConfig) -> KiboClientCore {
    KiboClientCore::new(c).unwrap()
}

fn plain(b: &[u8]) -> Vec<PlainItem> {
    vec![PlainItem::new(b.to_vec())]
}

async fn enc(core: &KiboClientCore, b: &[u8]) -> Result<CryptoBatch, k9o::KError> {
    core.encrypt("c", &plain(b), &CoreOptions::default()).await
}

fn count(l: &Log, prefix: &str) -> usize {
    entries(l).iter().filter(|e| e.starts_with(prefix)).count()
}

#[tokio::test]
async fn encrypt_token_then_exchange_then_seal_and_round_trips_through_decrypt() {
    let l = log();
    let core = built(cfg(&l));
    let r = core
        .encrypt("c", &[PlainItem::text("hi")], &CoreOptions::default())
        .await
        .unwrap();
    assert_eq!(
        entries(&l),
        vec![
            "token:c:encrypt".to_string(),
            "exchange:jwt-secret".to_string(),
            format!("seal:c:proj_1:{KEY}")
        ]
    );
    let ct = r.items[0].ciphertext.clone().unwrap();
    assert_eq!(STANDARD.decode(&ct).unwrap(), vec![1, 2, 3, b'h', b'i']);
    let d = core
        .decrypt("c", &[SealedItem::new(ct)], &CoreOptions::default())
        .await
        .unwrap();
    assert_eq!(d.items[0].plaintext_text().as_deref(), Some("hi"));
    assert!(entries(&l).contains(&"token:c:decrypt".to_string()));
}

#[tokio::test]
async fn declares_it_enforces_the_pin() {
    let l = log();
    assert!(built(cfg(&l)).enforces_expected_key_refs());
}

#[tokio::test]
async fn refuses_to_be_built_without_a_pin_or_with_a_key_outside_the_pin() {
    let l = log();
    let mut c = cfg(&l);
    c.expected_key_refs = vec![];
    let e = KiboClientCore::new(c).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert_eq!(e.context["field"], "expectedKeyRefs");

    let mut c = cfg(&l);
    c.expected_key_refs = vec!["arn:aws:kms:eu-central-1:111122223333:key/other".into()];
    let e = KiboClientCore::new(c).unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert_eq!(e.context["field"], "keyRef");

    let mut c = cfg(&l);
    c.provider = KeyRefProvider::Gcp;
    assert_eq!(
        KiboClientCore::new(c).unwrap_err().code,
        "K9O_INVALID_FIELD_VALUE"
    );
}

#[tokio::test]
async fn an_empty_pin_from_the_router_reaches_no_network_and_no_kms() {
    let l = log();
    let core = built(cfg(&l));
    let empty: [String; 0] = [];
    let opts = CoreOptions {
        expected_key_refs: Some(&empty),
    };
    let e = core
        .decrypt("c", &[SealedItem::new("AQID")], &opts)
        .await
        .unwrap_err();
    assert!(e.message.contains("key pin"), "{}", e.message);
    assert!(core.encrypt("c", &plain(&[1]), &opts).await.is_err());
    assert!(core
        .rewrap("c", &[SealedItem::new("AQID")], &opts)
        .await
        .is_err());
    assert!(entries(&l).is_empty());
}

#[tokio::test]
async fn the_router_pin_is_the_one_sent_to_the_core() {
    let l = log();
    let core = built(cfg(&l));
    let pin = vec![KEY.to_string(), "arn:aws:kms:eu-central-1:111122223333:key/two".to_string()];
    let opts = CoreOptions {
        expected_key_refs: Some(&pin),
    };
    core.encrypt("c", &plain(&[1]), &opts).await.unwrap();
    assert_eq!(
        entries(&l).last().unwrap(),
        &format!("seal:c:proj_1:{KEY},arn:aws:kms:eu-central-1:111122223333:key/two")
    );
}

#[tokio::test]
async fn a_core_that_does_not_list_expected_key_refs_is_refused_before_any_token() {
    let l = log();
    let mut f = fake(&l);
    f.caps = vec![];
    let core = built(cfg_with(&l, f, tok(Some("aws"), Some("proj_1"))));
    let e = enc(&core, &[1]).await.unwrap_err();
    assert_eq!(e.code, "K9O_CRYPTO_KEY_PIN_UNENFORCED");
    assert!(entries(&l).is_empty());
}

#[tokio::test]
async fn an_older_core_without_the_new_functions_is_refused() {
    let l = log();
    let mut f = fake(&l);
    f.missing = vec!["app_crypto_seal"];
    let core = built(cfg_with(&l, f, tok(Some("aws"), Some("proj_1"))));
    let e = enc(&core, &[1]).await.unwrap_err();
    assert_eq!(e.code, "KIBO_WASM_ERROR");
    assert!(entries(&l).is_empty());
}

#[tokio::test]
async fn a_response_naming_another_provider_or_project_is_refused_and_nothing_is_exchanged() {
    for t in [tok(Some("gcp"), Some("proj_1")), tok(Some("aws"), Some("proj_other"))] {
        let l = log();
        let core = built(cfg_with(&l, fake(&l), t));
        let e = enc(&core, &[1]).await.unwrap_err();
        assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
        assert_eq!(entries(&l), vec!["token:c:encrypt".to_string()]);
    }
}

#[tokio::test]
async fn project_id_comes_from_the_response_only_when_not_configured() {
    let l = log();
    let mut c = cfg(&l);
    c.project_id = None;
    let core = built(c);
    enc(&core, &[1]).await.unwrap();
    assert_eq!(entries(&l).last().unwrap(), &format!("seal:c:proj_1:{KEY}"));

    let l2 = log();
    let mut c = cfg_with(&l2, fake(&l2), tok(Some("aws"), None));
    c.project_id = None;
    let none = built(c);
    let e = enc(&none, &[1]).await.unwrap_err();
    assert_eq!(e.code, "K9O_INVALID_FIELD_VALUE");
    assert_eq!(count(&l2, "exchange"), 0);

    // A configured value is never overridden, and a response without the fields is accepted.
    let l3 = log();
    let core = built(cfg_with(&l3, fake(&l3), tok(None, None)));
    enc(&core, &[1]).await.unwrap();
    assert_eq!(entries(&l3).last().unwrap(), &format!("seal:c:proj_1:{KEY}"));
}

#[tokio::test]
async fn the_exchanged_credential_is_reused_until_near_expiry_then_exchanged_again() {
    let l = log();
    let clock = Arc::new(AtomicU64::new(1000));
    let mut c = cfg(&l);
    let ck = clock.clone();
    c.now = Arc::new(move || ck.load(Ordering::SeqCst));
    let core = built(c);
    enc(&core, &[1]).await.unwrap();
    enc(&core, &[1]).await.unwrap();
    assert_eq!(count(&l, "exchange"), 1);
    // A federated token is fetched on every call.
    assert_eq!(count(&l, "token"), 2);
    clock.store(9_950, Ordering::SeqCst); // inside the 60 s skew of 10_000
    enc(&core, &[1]).await.unwrap();
    assert_eq!(count(&l, "exchange"), 2);
}

#[tokio::test]
async fn the_credential_is_cached_per_crypt_op_and_subject() {
    let l = log();
    let core = built(cfg(&l));
    enc(&core, &[1]).await.unwrap();
    core.decrypt("c", &[SealedItem::new("AQID")], &CoreOptions::default())
        .await
        .unwrap();
    assert_eq!(count(&l, "exchange"), 2);
}

#[tokio::test]
async fn a_stalled_exchange_is_abandoned_at_the_deadline() {
    let l = log();
    let mut f = fake(&l);
    f.stall = true;
    let mut c = cfg_with(&l, f, tok(Some("aws"), Some("proj_1")));
    c.exchange_deadline = Duration::from_millis(20);
    let core = built(c);
    let e = enc(&core, &[1]).await.unwrap_err();
    assert_eq!(e.code, "KIBO_EXCHANGE_TIMEOUT");
    assert!(!format!("{e:?}{e}").contains("jwt-secret"));
}

#[tokio::test]
async fn one_bad_item_fails_on_its_own_and_carries_the_cores_code() {
    let l = log();
    let core = built(cfg(&l));
    let good = enc(&core, &[9]).await.unwrap().items[0].clone();
    let r = core
        .decrypt(
            "c",
            &[
                SealedItem::new("AAAA"),
                SealedItem::new(good.ciphertext.unwrap()),
                SealedItem::new("!!not base64!!"),
            ],
            &CoreOptions::default(),
        )
        .await
        .unwrap();
    assert!(r.items[0].is_error());
    assert_eq!(r.items[0].code.as_deref(), Some("K9O_CRYPTO_UNEXPECTED_KEY"));
    assert_eq!(r.items[0].message.as_deref(), Some("no"));
    assert!(!r.items[1].is_error());
    assert_eq!(r.items[1].plaintext_bytes(), Some(vec![9]));
    assert_eq!(r.items[2].code.as_deref(), Some("KIBO_WASM_ERROR"));
}

#[tokio::test]
async fn an_upstream_kms_failure_is_a_per_item_failure_not_a_call_error() {
    let l = log();
    let mut f = fake(&l);
    f.seal_err = Some("UPSTREAM_KMS_ERROR: denied".into());
    let core = built(cfg_with(&l, f, tok(Some("aws"), Some("proj_1"))));
    let r = core
        .encrypt(
            "c",
            &[PlainItem::new(vec![1]), PlainItem::new(vec![2])],
            &CoreOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(r.items.len(), 2);
    for it in &r.items {
        assert_eq!(it.code.as_deref(), Some("UPSTREAM_KMS_ERROR"));
        assert_eq!(it.message.as_deref(), Some("denied"));
    }
}

#[tokio::test]
async fn rewrap_opens_and_seals_under_the_configured_key_with_the_rewrap_token() {
    let l = log();
    let core = built(cfg(&l));
    let ct = enc(&core, &[7]).await.unwrap().items[0].ciphertext.clone().unwrap();
    l.lock().unwrap().clear();
    let r = core
        .rewrap("c", &[SealedItem::new(ct)], &CoreOptions::default())
        .await
        .unwrap();
    assert!(!r.items[0].is_error());
    assert_eq!(entries(&l)[0], "token:c:rewrap");
    let last: Vec<String> = entries(&l)
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(|e| e.split(':').next().unwrap().to_string())
        .collect();
    assert_eq!(last, vec!["open", "seal"]);
}

#[tokio::test]
async fn neither_the_token_nor_the_credential_appears_in_a_result_an_error_or_debug() {
    let l = log();
    let mut f = fake(&l);
    f.seal_err = Some("UPSTREAM_KMS_ERROR: denied".into());
    let core = built(cfg_with(&l, f, tok(Some("aws"), Some("proj_1"))));
    let r = core.encrypt("c", &plain(&[1]), &CoreOptions::default()).await.unwrap();
    let shown = format!("{r:?}{core:?}");
    assert!(!shown.contains("jwt-secret") && !shown.contains("cred-1"), "{shown}");

    let t = tok(Some("gcp"), None);
    let l2 = log();
    let e = enc(&built(cfg_with(&l2, fake(&l2), t.clone())), &[1]).await.unwrap_err();
    let shown = format!("{e:?}{e}{t:?}");
    assert!(!shown.contains("jwt-secret"), "{shown}");

    let cred = AppCredential {
        secret: "cred-1".into(),
        expires_at_unix_secs: 1,
    };
    let seal = AppSealInput {
        provider: k9o::KmsProvider::AwsKms,
        posture: Posture::Hyok,
        kms_cred: cred.clone(),
        key_ref: AppKeyRef { version: 1, external_ref: KEY.into() },
        provider_config: Default::default(),
        project_id: "p".into(),
        crypt: "c".into(),
        context: None,
        plaintext: vec![171, 205, 239],
        expected_key_refs: vec![KEY.into()],
    };
    let fed = FederationInput {
        spec: FederationSpec::Gcp { audience: "a".into() },
        identity_token: "jwt-secret".into(),
        now_unix_secs: 1,
    };
    let shown = format!("{cred:?}{seal:?}{fed:?}");
    assert!(!shown.contains("cred-1") && !shown.contains("jwt-secret"), "{shown}");
    assert!(!shown.contains("171, 205"), "plaintext bytes are not printed: {shown}");
}

#[test]
fn wire_names_are_the_cores() {
    let seal = AppSealInput {
        provider: k9o::KmsProvider::HashicorpVault,
        posture: Posture::Hyok,
        kms_cred: AppCredential { secret: "s".into(), expires_at_unix_secs: 5 },
        key_ref: AppKeyRef { version: 2, external_ref: "transit/k:2".into() },
        provider_config: Default::default(),
        project_id: "p".into(),
        crypt: "c".into(),
        context: None,
        plaintext: vec![],
        expected_key_refs: vec![],
    };
    let v = serde_json::to_value(&seal).unwrap();
    assert_eq!(v["provider"], "hashicorp-vault");
    assert_eq!(v["posture"], "hyok");
    assert_eq!(v["key_ref"]["external_ref"], "transit/k:2");
    let f = serde_json::to_value(FederationSpec::Vault {
        vault_addr: "a".into(),
        jwt_auth_mount: "jwt".into(),
        role: "r".into(),
    })
    .unwrap();
    assert_eq!(f["provider"], "vault");
    assert_eq!(f["jwt_auth_mount"], "jwt");
    assert!(f.get("mount").is_none());
}

#[tokio::test]
async fn a_per_call_pin_excluding_the_key_refuses_before_token_and_exchange() {
    let l = log();
    let core = built(cfg(&l));
    let other = vec!["arn:aws:kms:eu-central-1:111122223333:key/other".to_string()];
    let o = CoreOptions {
        expected_key_refs: Some(&other),
        ..CoreOptions::default()
    };
    let r = core.encrypt("c", &plain(b"x"), &o).await.unwrap();
    assert_eq!(r.items[0].code.as_deref(), Some("K9O_CRYPTO_UNEXPECTED_KEY"));
    let r = core
        .decrypt("c", &[SealedItem::new("AQID")], &o)
        .await
        .unwrap();
    assert_eq!(r.items[0].code.as_deref(), Some("K9O_CRYPTO_UNEXPECTED_KEY"));
    let r = core
        .rewrap("c", &[SealedItem::new("AQID")], &o)
        .await
        .unwrap();
    assert_eq!(r.items[0].code.as_deref(), Some("K9O_CRYPTO_UNEXPECTED_KEY"));
    assert!(entries(&l).is_empty(), "{:?}", entries(&l));

    let mine = vec![KEY.to_string()];
    let o = CoreOptions {
        expected_key_refs: Some(&mine),
        ..CoreOptions::default()
    };
    let r = core.encrypt("c", &plain(b"x"), &o).await.unwrap();
    assert!(!r.items[0].is_error());
    assert_eq!((count(&l, "token"), count(&l, "exchange"), count(&l, "seal")), (1, 1, 1));
}
