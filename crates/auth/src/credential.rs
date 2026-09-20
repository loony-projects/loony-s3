//! Credential lookup (architecture.md §54/prompt §54). `CredentialProvider` is a small
//! trait rather than a hard dependency on a concrete store, per the prompt's explicit
//! instruction ("design credential lookup through a trait") — the blanket impl below
//! means any `MetadataStore` (the credential system of record, see §37) already
//! satisfies it, with no adapter boilerplate at the call site.

use async_trait::async_trait;
use s3_core::OwnerId;
use s3_metadata::MetadataStore;

/// What SigV4 verification needs about one credential. Deliberately not the full
/// `s3_metadata::Credential` record (no `access_key`/`created_at` here) — this is the
/// minimal shape the signing-key derivation and the enabled/owner checks require.
#[derive(Debug, Clone)]
pub struct SigningCredential {
    pub secret_key: String,
    pub owner_id: OwnerId,
    pub enabled: bool,
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn get_secret(&self, access_key: &str) -> Option<SigningCredential>;
}

#[async_trait]
impl<T: MetadataStore + ?Sized> CredentialProvider for T {
    async fn get_secret(&self, access_key: &str) -> Option<SigningCredential> {
        let cred = self.get_credential(access_key).await.ok().flatten()?;
        Some(SigningCredential {
            secret_key: cred.secret_key,
            owner_id: cred.owner_id,
            enabled: cred.enabled,
        })
    }
}
