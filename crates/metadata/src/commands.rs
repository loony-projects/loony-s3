//! Command/query/response types for [`crate::MetadataStore`]. These are shaped so a
//! future Raft integration (Phase 8) can serialize the mutating ones as log entries
//! directly — see architecture.md §5/§26 for why Phase 2 doesn't wire real `openraft`
//! yet: with exactly one voter, a replicated log and a plain WAL are the same thing.

use std::collections::BTreeMap;

use s3_core::{
    BucketId, BucketName, ETag, NodeId, ObjectKey, OwnerId, PartManifest, UploadId, VolumeId,
};
use time::OffsetDateTime;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateBucket {
    pub name: BucketName,
    pub owner_id: OwnerId,
    pub region: String,
}

#[derive(Debug, Clone)]
pub struct ListObjectsQuery {
    pub bucket_id: BucketId,
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    pub start_after: Option<String>,
    pub continuation_token: Option<String>,
    pub max_keys: u32,
}

#[derive(Debug, Clone)]
pub struct ObjectSummary {
    pub key: ObjectKey,
    pub size: u64,
    pub etag: ETag,
    pub last_modified: OffsetDateTime,
}

#[derive(Debug, Clone, Default)]
pub struct ListObjectsPage {
    pub objects: Vec<ObjectSummary>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BeginMultipart {
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultipartUploadState {
    pub upload_id: UploadId,
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
    #[serde(with = "time::serde::rfc3339")]
    pub initiated_at: OffsetDateTime,
    pub parts: BTreeMap<u32, PartManifest>,
}

#[derive(Debug, Clone)]
pub struct PartSummary {
    pub part_number: u32,
    pub size: u64,
    pub etag_md5: [u8; 16],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompleteMultipart {
    pub upload_id: UploadId,
    /// Client-supplied ordered `(part_number, etag_hex)` list, validated against the
    /// parts actually recorded via `record_part` (prompt §57: `InvalidPart` /
    /// `InvalidPartOrder`).
    pub requested_parts: Vec<(u32, String)>,
    pub content_type: String,
}

/// Registers (or re-registers, bumping `generation`) a node in the cluster's registry
/// (architecture.md §34).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisterNode {
    pub node_id: NodeId,
    pub advertised_address: String,
    pub failure_domain: Vec<String>,
    pub volumes: Vec<VolumeId>,
}

/// A credential record (architecture.md §54). `secret_key` is stored as configured
/// (encrypted at rest once a KMS/envelope key is wired up — not yet in this phase),
/// never hashed: SigV4 verification needs to recompute an HMAC from the actual secret,
/// which a one-way hash can't support (architecture.md §24).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Credential {
    pub access_key: String,
    pub secret_key: String,
    pub owner_id: OwnerId,
    pub enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}
