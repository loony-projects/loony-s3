//! The `MetadataStore` trait (architecture.md §4/§37). The authoritative, transactional
//! source of truth for buckets, objects/manifests, and multipart uploads. `cluster`
//! and node/credential-registry methods are added in later phases as those subsystems
//! land; this phase covers what Phase 3 (Standalone S3) needs.

use async_trait::async_trait;
use s3_core::{Bucket, BucketId, BucketName, ObjectKey, ObjectManifest, OwnerId, UploadId};

use crate::commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, Credential, ListObjectsPage, ListObjectsQuery,
    PartSummary,
};
use crate::error::MetaError;
use s3_core::PartManifest;

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_bucket(&self, cmd: CreateBucket) -> Result<Bucket, MetaError>;
    async fn delete_bucket(&self, name: &BucketName) -> Result<(), MetaError>;
    async fn get_bucket(&self, name: &BucketName) -> Result<Option<Bucket>, MetaError>;
    async fn list_buckets(&self, owner: OwnerId) -> Result<Vec<Bucket>, MetaError>;

    /// Atomically make `manifest` the current version of its `(bucket_id, key)` — the
    /// visibility point every PUT/CompleteMultipart converges on (architecture.md §21).
    async fn commit_manifest(&self, manifest: ObjectManifest) -> Result<ObjectManifest, MetaError>;
    async fn get_manifest(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectManifest>, MetaError>;
    /// Idempotent: tombstoning an already-absent object is not an error.
    async fn tombstone_object(&self, bucket_id: BucketId, key: &ObjectKey)
    -> Result<(), MetaError>;
    async fn list_objects(&self, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError>;

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError>;
    async fn record_part(&self, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError>;
    async fn list_parts(&self, upload_id: UploadId) -> Result<Vec<PartSummary>, MetaError>;
    async fn complete_multipart(&self, cmd: CompleteMultipart)
    -> Result<ObjectManifest, MetaError>;
    /// Idempotent: aborting an already-aborted/completed/unknown upload is not an error.
    async fn abort_multipart(&self, upload_id: UploadId) -> Result<(), MetaError>;

    async fn put_credential(&self, cred: Credential) -> Result<(), MetaError>;
    async fn get_credential(&self, access_key: &str) -> Result<Option<Credential>, MetaError>;
}
