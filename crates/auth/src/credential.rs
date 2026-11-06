//! Credential lookup (architecture.md §54/prompt §54). `CredentialProvider` is a small
//! trait rather than a hard dependency on a concrete store, per the prompt's explicit
//! instruction ("design credential lookup through a trait") — the blanket impl below
//! means any `MetadataStore` (the credential system of record, see §37) already
//! satisfies it, with no adapter boilerplate at the call site.

use async_trait::async_trait;
use loony_core::{OwnerId, UserId};
use loony_metadata::{CredentialKind, MetadataStore};
use time::OffsetDateTime;

/// What SigV4 verification needs about one credential. Deliberately not the full
/// `loony_metadata::Credential` record (no `access_key`/`created_at` here) — this is the
/// minimal shape the signing-key derivation and the enabled/owner checks require.
#[derive(Debug, Clone)]
pub struct SigningCredential {
    pub secret_key: String,
    pub owner_id: OwnerId,
    pub enabled: bool,
    pub kind: CredentialKind,
    pub user_id: Option<UserId>,
    pub expires_at: Option<OffsetDateTime>,
    pub last_used_at: Option<OffsetDateTime>,
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn get_secret(&self, access_key: &str) -> Option<SigningCredential>;

    /// Records that a key was just used. Best effort: losing this must never fail a request.
    async fn record_use(&self, _access_key: &str, _at: OffsetDateTime) {}
}

#[async_trait]
impl<T: MetadataStore + ?Sized> CredentialProvider for T {
    async fn get_secret(&self, access_key: &str) -> Option<SigningCredential> {
        let cred = self.get_credential(access_key).await.ok().flatten()?;
        // Checked here rather than recorded on each key, so a project coming back doesn't
        // also switch back on the keys its members had turned off.
        let project_enabled = match cred.project_id {
            Some(project_id) => self
                .get_project(project_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|project| project.enabled),
            None => true,
        };
        Some(SigningCredential {
            secret_key: cred.secret_key,
            owner_id: cred.owner_id,
            enabled: cred.enabled && project_enabled,
            kind: cred.kind,
            user_id: cred.user_id,
            expires_at: cred.expires_at,
            last_used_at: cred.last_used_at,
        })
    }

    async fn record_use(&self, access_key: &str, at: OffsetDateTime) {
        // Only the metadata leader can write, so this routinely fails on other nodes.
        if let Err(err) = self.touch_credential(access_key, at).await {
            tracing::debug!(%err, "couldn't record that a key was used");
        }
    }
}
