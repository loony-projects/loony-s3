//! The internal RPC client: a [`ShardStore`] implementation that dispatches to a
//! *remote* node over HTTP instead of the local disk. Combined with
//! `s3-storage`'s `LocalVolumeManager`, this is the "local vs remote" split
//! architecture.md §2 describes — nothing above the `ShardStore` trait needs to know
//! which one it's talking to.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use s3_core::{NodeId, ShardId, ShardReceipt, ShardStat, ShardTarget};
use s3_storage::{ShardBytesIn, ShardBytesOut, ShardStore, StorageError};

/// Maps a node's persistent identity to the base URL of its internal RPC server.
/// Phase 6 scope: nothing here yet tracks cluster membership dynamically (that's
/// `s3-cluster`'s job from Phase 7 on) — [`StaticResolver`] below is a fixed map,
/// useful for tests and any deployment that configures peers by hand.
pub trait NodeAddressResolver: Send + Sync {
    fn resolve(&self, node_id: NodeId) -> Option<String>;
}

pub struct StaticResolver {
    addresses: HashMap<NodeId, String>,
}

impl StaticResolver {
    pub fn new(addresses: HashMap<NodeId, String>) -> Self {
        Self { addresses }
    }
}

impl NodeAddressResolver for StaticResolver {
    fn resolve(&self, node_id: NodeId) -> Option<String> {
        self.addresses.get(&node_id).cloned()
    }
}

pub struct RemoteShardStore {
    http: reqwest::Client,
    resolver: Arc<dyn NodeAddressResolver>,
    token: String,
}

impl RemoteShardStore {
    pub fn new(resolver: Arc<dyn NodeAddressResolver>, token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            resolver,
            token,
        }
    }

    fn base_url(&self, node_id: NodeId) -> Result<String, StorageError> {
        self.resolver.resolve(node_id).ok_or_else(|| {
            StorageError::Unreachable(node_id, "no known address for this node".into())
        })
    }

    /// This node's own health, as reported over the RPC transport (not a local
    /// shortcut) — useful for connectivity checks during join/heartbeat (Phase 7).
    pub async fn health(&self, node_id: NodeId) -> Result<(), StorageError> {
        let base = self.base_url(node_id)?;
        let resp = self
            .http
            .get(format!("{base}/internal/v1/health"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| StorageError::Unreachable(node_id, e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(StorageError::Remote(
                node_id,
                format!("health check returned {}", resp.status()),
            ))
        }
    }
}

#[async_trait]
impl ShardStore for RemoteShardStore {
    async fn put_shard(
        &self,
        target: ShardTarget,
        data: ShardBytesIn,
    ) -> Result<ShardReceipt, StorageError> {
        let base = self.base_url(target.node_id)?;
        let url = format!("{base}/internal/v1/volumes/{}/shards", target.volume_id);
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.token)
            .body(reqwest::Body::wrap_stream(data))
            .send()
            .await
            .map_err(|e| StorageError::Unreachable(target.node_id, e.to_string()))?;
        if !resp.status().is_success() {
            return Err(StorageError::Remote(
                target.node_id,
                format!("PutShard returned {}", resp.status()),
            ));
        }
        resp.json::<ShardReceipt>().await.map_err(|e| {
            StorageError::Remote(target.node_id, format!("malformed PutShard response: {e}"))
        })
    }

    async fn get_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<ShardBytesOut, StorageError> {
        let base = self.base_url(target.node_id)?;
        let url = format!(
            "{base}/internal/v1/volumes/{}/shards/{shard_id}",
            target.volume_id
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| StorageError::Unreachable(target.node_id, e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(shard_id));
        }
        if !resp.status().is_success() {
            return Err(StorageError::Remote(
                target.node_id,
                format!("GetShard returned {}", resp.status()),
            ));
        }
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        Ok(Box::pin(stream))
    }

    async fn stat_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<Option<ShardStat>, StorageError> {
        let base = self.base_url(target.node_id)?;
        let url = format!(
            "{base}/internal/v1/volumes/{}/shards/{shard_id}",
            target.volume_id
        );
        let resp = self
            .http
            .head(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| StorageError::Unreachable(target.node_id, e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(StorageError::Remote(
                target.node_id,
                format!("StatShard returned {}", resp.status()),
            ));
        }
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(Some(ShardStat { size }))
    }

    async fn delete_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<(), StorageError> {
        let base = self.base_url(target.node_id)?;
        let url = format!(
            "{base}/internal/v1/volumes/{}/shards/{shard_id}",
            target.volume_id
        );
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| StorageError::Unreachable(target.node_id, e.to_string()))?;
        // Idempotent: a 404 here means the shard is already gone, which is success.
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(StorageError::Remote(
                target.node_id,
                format!("DeleteShard returned {}", resp.status()),
            ))
        }
    }
}
