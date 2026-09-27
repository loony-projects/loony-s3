//! Object manifest (architecture.md §6): the authoritative description of an object's
//! physical representation. Never expose `ShardLocation`/physical identifiers through
//! the LS3 API — only `object`/`api` see this far down.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::etag::ETag;
use crate::ids::{BucketId, NodeId, ObjectId, ShardId, VersionId, VolumeId};
use crate::object_key::ObjectKey;

/// How a stripe's bytes are made durable. `Replicated { n: 1 }` (Phase 3's baseline: one
/// copy, no redundancy yet) and `Replicated { n: 3 }` (Phase 5's small-object policy,
/// architecture.md §9) are the same mechanism at different replica counts;
/// `Erasure { data, parity }` is Phase 5's large-object policy (architecture.md §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DurabilityPolicy {
    Replicated { n: u8 },
    Erasure { data: u8, parity: u8 },
}

impl DurabilityPolicy {
    /// Total number of shards a stripe under this policy has.
    pub fn total_shards(&self) -> u8 {
        match *self {
            DurabilityPolicy::Replicated { n } => n,
            DurabilityPolicy::Erasure { data, parity } => data + parity,
        }
    }

    /// Minimum number of shards needed to reconstruct a stripe's bytes.
    pub fn min_reconstructable(&self) -> u8 {
        match *self {
            DurabilityPolicy::Replicated { .. } => 1,
            DurabilityPolicy::Erasure { data, .. } => data,
        }
    }
}

/// Where one shard of one stripe physically lives (architecture.md §10 fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardLocation {
    pub shard_index: u16,
    pub node_id: NodeId,
    pub volume_id: VolumeId,
    pub shard_id: ShardId,
    pub size: u64,
    pub checksum: [u8; 32],
    pub generation: u64,
}

/// One independently encoded/replicated stripe of a part's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stripe {
    pub stripe_index: u32,
    pub stripe_offset: u64,
    pub stripe_len: u64,
    pub durability: DurabilityPolicy,
    pub shards: Vec<ShardLocation>,
}

/// The durable representation of one part (a whole non-multipart PUT is a single part).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartManifest {
    pub part_number: u32,
    pub offset: u64,
    pub size: u64,
    pub etag_md5: [u8; 16],
    pub stripes: Vec<Stripe>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectManifest {
    pub object_id: ObjectId,
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub version_id: VersionId,
    pub size: u64,
    pub etag: ETag,
    pub sha256: [u8; 32],
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub delete_marker: bool,
    pub parts: Vec<PartManifest>,
}

impl ObjectManifest {
    pub fn total_size(&self) -> u64 {
        self.parts.iter().map(|p| p.size).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_policy_shard_counts() {
        let p = DurabilityPolicy::Replicated { n: 3 };
        assert_eq!(p.total_shards(), 3);
        assert_eq!(p.min_reconstructable(), 1);
    }

    #[test]
    fn erasure_policy_shard_counts() {
        let p = DurabilityPolicy::Erasure { data: 4, parity: 2 };
        assert_eq!(p.total_shards(), 6);
        assert_eq!(p.min_reconstructable(), 4);
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let manifest = ObjectManifest {
            object_id: ObjectId::new(),
            bucket_id: BucketId::new(),
            key: ObjectKey::parse("a/b").unwrap(),
            version_id: VersionId::new(),
            size: 3,
            etag: ETag::from_md5([1u8; 16]),
            sha256: [2u8; 32],
            content_type: "application/octet-stream".into(),
            user_metadata: BTreeMap::new(),
            created_at: OffsetDateTime::now_utc(),
            delete_marker: false,
            parts: vec![PartManifest {
                part_number: 1,
                offset: 0,
                size: 3,
                etag_md5: [1u8; 16],
                stripes: vec![Stripe {
                    stripe_index: 0,
                    stripe_offset: 0,
                    stripe_len: 3,
                    durability: DurabilityPolicy::Replicated { n: 1 },
                    shards: vec![ShardLocation {
                        shard_index: 0,
                        node_id: NodeId::new(),
                        volume_id: VolumeId::new(),
                        shard_id: ShardId::new(),
                        size: 3,
                        checksum: [3u8; 32],
                        generation: 0,
                    }],
                }],
            }],
        };

        let json = serde_json::to_vec(&manifest).unwrap();
        let parsed: ObjectManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.total_size(), 3);
    }
}
