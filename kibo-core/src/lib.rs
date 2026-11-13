//! The Kibo core adapter for the k9o SDK.
//!
//! [`KiboCore`] implements [`k9o::AppCore`] over `kibo-kms-rest-ffi` (`zk-bundle-v2.16.3` or
//! later): it hands the SDK's plain structs to the core's `app_crypto_seal`, `app_crypto_open`
//! and `acquire_kms_credential`, and gives the core's errors back as `"<CODE>: <message>"`.
//! There is no cryptography here. Sealing, opening, the key pin and the token exchange all run in
//! the core; this crate only converts types.
//!
//! ```ignore
//! let core = k9o_kibo_core::KiboCore::new();
//! let cfg = k9o::KiboClientCoreConfig::new(provider, posture, key_ref, federation, pin, token_source, Arc::new(core));
//! k9o.crypto().use_client_core(Arc::new(k9o::KiboClientCore::new(cfg)?));
//! ```

use async_trait::async_trait;
use k9o::{
    AppCore, AppCredential, AppOpenInput, AppSealInput, FederationInput, FederationSpec,
    KmsProvider, Posture, ProviderConfig,
};
use kibo_kms_rest_ffi as core;

/// The real Kibo core as an [`AppCore`]. Holds no state; clone or share it freely.
#[derive(Debug, Clone, Copy, Default)]
pub struct KiboCore;

impl KiboCore {
    pub fn new() -> Self {
        Self
    }
}

fn provider(p: KmsProvider) -> core::KmsProvider {
    match p {
        KmsProvider::AwsKms => core::KmsProvider::AwsKms,
        KmsProvider::GcpKms => core::KmsProvider::GcpKms,
        KmsProvider::AzureKv => core::KmsProvider::AzureKv,
        KmsProvider::HashicorpVault => core::KmsProvider::HashicorpVault,
    }
}

fn posture(p: Posture) -> core::FfiPosture {
    match p {
        Posture::Byok => core::FfiPosture::Byok,
        Posture::Hyok => core::FfiPosture::Hyok,
    }
}

fn credential(c: &AppCredential) -> core::FfiCredential {
    core::FfiCredential {
        secret: c.secret.clone(),
        expires_at_unix_secs: c.expires_at_unix_secs,
    }
}

fn provider_config(c: &ProviderConfig) -> core::FfiProviderConfig {
    core::FfiProviderConfig {
        aws_region: c.aws_region.clone(),
        vault_address: c.vault_address.clone(),
    }
}

/// The SDK's federation spec as the core's. The SDK's `jwt_auth_mount` is the core's `mount`;
/// this is the one place the two names meet.
pub fn federation_spec(spec: &FederationSpec) -> core::types::FfiFederationSpec {
    use core::types::FfiFederationSpec as C;
    match spec {
        FederationSpec::Aws { role_arn, session_name, region, key_arn, duration_secs } => C::Aws {
            role_arn: role_arn.clone(),
            session_name: session_name.clone(),
            region: region.clone(),
            key_arn: key_arn.clone(),
            duration_secs: *duration_secs,
        },
        FederationSpec::Gcp { audience } => C::Gcp { audience: audience.clone() },
        FederationSpec::Azure { tenant_id, client_id, scope } => C::Azure {
            tenant_id: tenant_id.clone(),
            client_id: client_id.clone(),
            scope: scope.clone(),
        },
        FederationSpec::Vault { vault_addr, jwt_auth_mount, role } => C::Vault {
            vault_addr: vault_addr.clone(),
            mount: jwt_auth_mount.clone(),
            role: role.clone(),
        },
    }
}

fn seal_input(i: AppSealInput) -> core::AppSealInput {
    core::AppSealInput {
        provider: provider(i.provider),
        posture: posture(i.posture),
        kms_cred: credential(&i.kms_cred),
        key_ref: core::FfiKeyRef { version: i.key_ref.version, external_ref: i.key_ref.external_ref },
        provider_config: provider_config(&i.provider_config),
        project_id: i.project_id,
        crypt: i.crypt,
        context: i.context,
        plaintext: i.plaintext,
        expected_key_refs: i.expected_key_refs,
    }
}

fn open_input(i: AppOpenInput) -> core::AppOpenInput {
    core::AppOpenInput {
        kms_cred: credential(&i.kms_cred),
        provider_config: provider_config(&i.provider_config),
        project_id: i.project_id,
        crypt: i.crypt,
        context: i.context,
        container: i.container,
        expected_key_refs: i.expected_key_refs,
    }
}

#[async_trait]
impl AppCore for KiboCore {
    fn capabilities(&self) -> Vec<String> {
        core::core_capabilities()
    }

    async fn seal(&self, input: AppSealInput) -> Result<Vec<u8>, String> {
        core::app_crypto_seal(seal_input(input))
            .await
            .map(|o| o.container)
            .map_err(|e| e.to_string())
    }

    async fn open(&self, input: AppOpenInput) -> Result<Vec<u8>, String> {
        core::app_crypto_open(open_input(input))
            .await
            .map(|o| o.plaintext)
            .map_err(|e| e.to_string())
    }

    async fn acquire(&self, input: FederationInput) -> Result<AppCredential, String> {
        let i = core::types::FfiFederationInput {
            spec: federation_spec(&input.spec),
            identity_token: input.identity_token.clone(),
            now_unix_secs: input.now_unix_secs,
        };
        core::federate::acquire_kms_credential(i)
            .await
            .map(|c| AppCredential { secret: c.secret.clone(), expires_at_unix_secs: c.expires_at_unix_secs })
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_auth_mount_becomes_the_cores_mount() {
        let c = federation_spec(&FederationSpec::Vault {
            vault_addr: "https://v".into(),
            jwt_auth_mount: "jwt".into(),
            role: "r".into(),
        });
        match c {
            core::types::FfiFederationSpec::Vault { vault_addr, mount, role } => {
                assert_eq!((vault_addr.as_str(), mount.as_str(), role.as_str()), ("https://v", "jwt", "r"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_core_lists_the_pin() {
        assert!(KiboCore::new().capabilities().iter().any(|c| c == "expected_key_refs"));
    }
}
