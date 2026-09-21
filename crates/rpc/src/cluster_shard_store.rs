//! Phase 9: a [`ShardStore`] that routes each call to local disk or a remote node over
//! RPC based on the target's `node_id` — the piece that lets `s3-object`'s PUT/GET
//! treat "shard lives on this node" and "shard lives on some other node" identically,
//! the way `RemoteShardStore`'s own doc comment always intended once something above it
//! actually chose remote targets (Phase 6). Nothing in `s3-object` needed to change for
//! GET to become cluster-aware — it already built a [`ShardTarget`] from whatever
//! `node_id`/`volume_id` the manifest recorded; only PUT's placement decision and this
//! dispatcher were missing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use s3_core::{NodeId, NodeInfo, ShardId, ShardReceipt, ShardStat, ShardTarget};
use s3_storage::{ShardBytesIn, ShardBytesOut, ShardStore, StorageError};

use crate::client::{NodeAddressResolver, RemoteShardStore};

/// A [`NodeAddressResolver`] backed by a periodically-refreshed snapshot of the
/// (Raft-replicated, as of Phase 8) node registry, rather than a fixed map. `resolve()`
/// itself stays synchronous and I/O-free — `refresh()` is what does the actual
/// `MetadataStore::list_nodes()` call, meant to be driven by a background task the same
/// way `s3_cluster::run_heartbeat_loop` already drives failure detection.
#[derive(Clone, Default)]
pub struct CachedNodeResolver {
    addresses: Arc<RwLock<HashMap<NodeId, String>>>,
}

impl CachedNodeResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the cached address map with every currently-known node's advertised
    /// address, regardless of health -- staleness is handled by the caller simply
    /// getting an `Unreachable` error from a dead node's address, not by hiding it here.
    /// `NodeInfo::advertised_address` is a bare `host:port` (that's what `--advertise-addr`
    /// takes), but `RemoteShardStore` builds full request URLs from what this resolves
    /// to, so a scheme gets added here -- the same normalization `main.rs` already does
    /// for `--join`'s seed address.
    pub fn refresh(&self, nodes: &[NodeInfo]) {
        let map = nodes
            .iter()
            .map(|n| (n.node_id, with_scheme(&n.advertised_address)))
            .collect();
        *self.addresses.write().unwrap_or_else(|e| e.into_inner()) = map;
    }
}

fn with_scheme(addr: &str) -> String {
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

impl NodeAddressResolver for CachedNodeResolver {
    fn resolve(&self, node_id: NodeId) -> Option<String> {
        self.addresses
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node_id)
            .cloned()
    }
}

/// Dispatches to `local` when a target's `node_id` is this node's own, `remote`
/// otherwise. `local` and `remote` each already implement the full [`ShardStore`]
/// surface (Phase 1 and Phase 6 respectively); this only ever forwards, never
/// reimplements shard I/O itself.
pub struct ClusterShardStore {
    local_node_id: NodeId,
    local: Arc<dyn ShardStore>,
    remote: RemoteShardStore,
}

impl ClusterShardStore {
    pub fn new(
        local_node_id: NodeId,
        local: Arc<dyn ShardStore>,
        resolver: Arc<dyn NodeAddressResolver>,
        token: String,
    ) -> Self {
        Self {
            local_node_id,
            local,
            remote: RemoteShardStore::new(resolver, token),
        }
    }

    fn backend(&self, target: ShardTarget) -> &dyn ShardStore {
        if target.node_id == self.local_node_id {
            self.local.as_ref()
        } else {
            &self.remote
        }
    }
}

#[async_trait]
impl ShardStore for ClusterShardStore {
    async fn put_shard(
        &self,
        target: ShardTarget,
        data: ShardBytesIn,
    ) -> Result<ShardReceipt, StorageError> {
        self.backend(target).put_shard(target, data).await
    }

    async fn get_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<ShardBytesOut, StorageError> {
        self.backend(target).get_shard(target, shard_id).await
    }

    async fn stat_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<Option<ShardStat>, StorageError> {
        self.backend(target).stat_shard(target, shard_id).await
    }

    async fn delete_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<(), StorageError> {
        self.backend(target).delete_shard(target, shard_id).await
    }
}
