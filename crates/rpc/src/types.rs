//! Wire types shared between the RPC client and server for cluster operations
//! (`ClusterJoin`, `Health` — prompt §40). Shard operations don't need DTOs of their
//! own: `s3_core::ShardReceipt`/`ShardStat` already derive `Serialize`/`Deserialize`
//! and are used directly as JSON bodies.

use s3_core::{NodeId, NodeInfo, VolumeId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    pub node_id: NodeId,
    pub protocol_version: u32,
    pub status: String,
}

/// What a joining node sends to a seed. `claimed_cluster_id` is `None` for a node that
/// has never belonged to any cluster before; `Some` lets the seed refuse a request that
/// would otherwise silently merge two unrelated clusters (architecture.md §36).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub node_id: NodeId,
    pub advertised_address: String,
    pub failure_domain: Vec<String>,
    pub claimed_cluster_id: Option<String>,
    /// This node's local shard-storage volumes, so the placement engine's cluster-wide
    /// candidate pool (architecture.md §10) includes them as soon as it joins.
    pub volumes: Vec<VolumeId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    pub cluster_id: String,
    pub members: Vec<NodeInfo>,
}
