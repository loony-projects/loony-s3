//! The `ShardStore` trait (architecture.md §4/§2): put/get/stat/delete a single opaque
//! shard, addressed by `ShardTarget`. `storage` provides the local-disk
//! implementation (`local::LocalVolumeManager`); a remote (RPC-dispatching)
//! implementation is added in Phase 6 once `rpc` exists. Nothing above this trait —
//! placement, erasure coding, the object service — knows or cares which one it's
//! talking to.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use loony_core::{ShardId, ShardReceipt, ShardStat, ShardTarget};

use crate::error::StorageError;

/// A stream of shard bytes being written in. Bounded/streamed rather than a single
/// `Bytes` buffer so a caller never has to hold a whole object (let alone a whole
/// shard) in memory at once (architecture.md §4 priority: streaming I/O, §81 memory
/// requirement).
pub type ShardBytesIn = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// A stream of shard bytes being read back out.
pub type ShardBytesOut = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

#[async_trait]
pub trait ShardStore: Send + Sync {
    /// Write a new shard to `target`. The store mints the `ShardId` and computes the
    /// checksum while streaming — callers never choose the shard id themselves.
    async fn put_shard(
        &self,
        target: ShardTarget,
        data: ShardBytesIn,
    ) -> Result<ShardReceipt, StorageError>;

    async fn get_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<ShardBytesOut, StorageError>;

    async fn stat_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<Option<ShardStat>, StorageError>;

    async fn delete_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<(), StorageError>;
}
