//! Types describing where a shard lives and what came back from writing/reading one.
//! Pure data — no I/O lives in this crate (architecture.md §3).

use serde::{Deserialize, Serialize};

use crate::ids::{NodeId, ShardId, VolumeId};

/// Identifies exactly one physical shard-storage destination: a volume on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShardTarget {
    pub node_id: NodeId,
    pub volume_id: VolumeId,
}

/// Returned by a successful shard write. The `ShardStore` implementation mints
/// `shard_id` and computes `checksum` incrementally while streaming the bytes to disk —
/// callers never choose the shard id themselves (architecture.md §7/§10 record this
/// receipt into an `ObjectManifest`'s `ShardLocation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardReceipt {
    pub shard_id: ShardId,
    pub checksum: [u8; 32],
    pub size: u64,
}

/// Cheap existence/size check. Deliberately does not include a checksum: recomputing a
/// shard's hash is exactly as expensive as reading it, so checksum verification belongs
/// to the read path (which is already streaming the bytes), not to `stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardStat {
    pub size: u64,
}
