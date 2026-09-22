//! Local disk shard engine (architecture.md §7): one [`LocalVolume`] per physical disk,
//! using a hash-prefixed, content-addressed-by-random-id layout and the atomic
//! create-tmp / stream+checksum / fsync / rename write protocol. [`LocalVolumeManager`]
//! owns every volume on this node and implements [`ShardStore`] by dispatching on
//! `ShardTarget`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use loony_core::{NodeId, ShardId, ShardReceipt, ShardStat, ShardTarget, VolumeId};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::atomic::sync_parent_dir;
use crate::error::StorageError;
use crate::store::{ShardBytesIn, ShardBytesOut, ShardStore};
use crate::volume::VolumeMeta;

/// A single physical disk. Layout under `root`:
/// ```text
/// VOLUME_META
/// shards/<2-hex>/<2-hex>/<shard-id>
/// tmp/<shard-id>.tmp
/// ```
pub struct LocalVolume {
    root: PathBuf,
    meta: VolumeMeta,
}

impl LocalVolume {
    /// Open (creating if necessary) the volume rooted at `volume_root`, then sweep any
    /// temp files left behind by a crash on a previous run.
    pub async fn open(
        volume_root: impl Into<PathBuf>,
        local_node: NodeId,
    ) -> Result<Self, StorageError> {
        let root = volume_root.into();
        let meta = VolumeMeta::load_or_create(&root, local_node).await?;
        tokio::fs::create_dir_all(root.join("shards")).await?;
        tokio::fs::create_dir_all(root.join("tmp")).await?;

        let volume = Self { root, meta };
        volume.sweep_tmp().await?;
        Ok(volume)
    }

    pub fn volume_id(&self) -> VolumeId {
        self.meta.volume_id
    }

    fn shard_path(&self, id: ShardId) -> PathBuf {
        let hex = id.as_uuid().simple().to_string();
        self.root
            .join("shards")
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(&hex)
    }

    fn tmp_path(&self, id: ShardId) -> PathBuf {
        self.root
            .join("tmp")
            .join(format!("{}.tmp", id.as_uuid().simple()))
    }

    /// Delete every file left in `tmp/`. Safe unconditionally: nothing outside
    /// [`Self::put_shard`]'s own short write window ever creates a file there, and that
    /// window never spans a restart, so anything found at startup is a crash remnant
    /// (architecture.md §7/§50). A future GC phase (§63/§64) will age-gate this instead
    /// of sweeping everything, once shard writes can be issued by more than one path.
    pub async fn sweep_tmp(&self) -> Result<usize, StorageError> {
        let mut dir = tokio::fs::read_dir(self.root.join("tmp")).await?;
        let mut removed = 0usize;
        while let Some(entry) = dir.next_entry().await? {
            if entry.file_type().await?.is_file() {
                tokio::fs::remove_file(entry.path()).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub async fn put_shard(
        &self,
        mut data: impl Stream<Item = std::io::Result<Bytes>> + Unpin,
    ) -> Result<ShardReceipt, StorageError> {
        let shard_id = ShardId::new();
        let tmp_path = self.tmp_path(shard_id);

        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let checksum: [u8; 32] = hasher.finalize().into();
        let final_path = self.shard_path(shard_id);
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&tmp_path, &final_path).await?;
        sync_parent_dir(&final_path).await?;

        Ok(ShardReceipt {
            shard_id,
            checksum,
            size,
        })
    }

    pub async fn get_shard(
        &self,
        shard_id: ShardId,
    ) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, StorageError> {
        let path = self.shard_path(shard_id);
        let file = tokio::fs::File::open(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(shard_id)
            } else {
                e.into()
            }
        })?;
        Ok(tokio_util::io::ReaderStream::new(file))
    }

    pub async fn stat_shard(&self, shard_id: ShardId) -> Result<Option<ShardStat>, StorageError> {
        match tokio::fs::metadata(self.shard_path(shard_id)).await {
            Ok(m) => Ok(Some(ShardStat { size: m.len() })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Deleting an already-absent shard is not an error: GC/healing/rebalancing all
    /// retry delete operations, and a delete must be idempotent (architecture.md §44).
    pub async fn delete_shard(&self, shard_id: ShardId) -> Result<(), StorageError> {
        match tokio::fs::remove_file(self.shard_path(shard_id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Every volume this node owns. Implements [`ShardStore`] by routing on `ShardTarget`;
/// a `target.node_id` that isn't `local_node`, or a `target.volume_id` this node doesn't
/// have, is a caller bug (bad placement plan) and is reported as such rather than
/// silently ignored.
pub struct LocalVolumeManager {
    local_node: NodeId,
    volumes: HashMap<VolumeId, LocalVolume>,
}

impl LocalVolumeManager {
    pub async fn open(
        local_node: NodeId,
        volume_roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, StorageError> {
        let mut volumes = HashMap::new();
        for root in volume_roots {
            let volume = LocalVolume::open(root, local_node).await?;
            volumes.insert(volume.volume_id(), volume);
        }
        Ok(Self {
            local_node,
            volumes,
        })
    }

    pub fn volume_ids(&self) -> impl Iterator<Item = VolumeId> + '_ {
        self.volumes.keys().copied()
    }

    fn volume(&self, target: ShardTarget) -> Result<&LocalVolume, StorageError> {
        if target.node_id != self.local_node {
            return Err(StorageError::NotLocal {
                target,
                local_node: self.local_node,
            });
        }
        self.volumes
            .get(&target.volume_id)
            .ok_or(StorageError::UnknownVolume(target.volume_id))
    }
}

#[async_trait]
impl ShardStore for LocalVolumeManager {
    async fn put_shard(
        &self,
        target: ShardTarget,
        data: ShardBytesIn,
    ) -> Result<ShardReceipt, StorageError> {
        self.volume(target)?.put_shard(data).await
    }

    async fn get_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<ShardBytesOut, StorageError> {
        let stream = self.volume(target)?.get_shard(shard_id).await?;
        Ok(Box::pin(stream)
            as Pin<
                Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>,
            >)
    }

    async fn stat_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<Option<ShardStat>, StorageError> {
        self.volume(target)?.stat_shard(shard_id).await
    }

    async fn delete_shard(
        &self,
        target: ShardTarget,
        shard_id: ShardId,
    ) -> Result<(), StorageError> {
        self.volume(target)?.delete_shard(shard_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn bytes_stream(
        chunks: Vec<&'static [u8]>,
    ) -> impl Stream<Item = std::io::Result<Bytes>> + Unpin {
        stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn put_then_get_roundtrips_bytes_and_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeId::new();
        let volume = LocalVolume::open(dir.path(), node).await.unwrap();

        let receipt = volume
            .put_shard(bytes_stream(vec![b"hello ", b"world"]))
            .await
            .unwrap();
        assert_eq!(receipt.size, 11);

        let mut out = volume.get_shard(receipt.shard_id).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = out.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"hello world");

        let expected_checksum: [u8; 32] = Sha256::digest(b"hello world").into();
        assert_eq!(receipt.checksum, expected_checksum);
    }

    #[tokio::test]
    async fn get_missing_shard_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let volume = LocalVolume::open(dir.path(), NodeId::new()).await.unwrap();

        let err = volume
            .get_shard(ShardId::new())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn stat_reflects_size_and_absence() {
        let dir = tempfile::tempdir().unwrap();
        let volume = LocalVolume::open(dir.path(), NodeId::new()).await.unwrap();

        assert_eq!(volume.stat_shard(ShardId::new()).await.unwrap(), None);

        let receipt = volume.put_shard(bytes_stream(vec![b"abc"])).await.unwrap();
        let stat = volume.stat_shard(receipt.shard_id).await.unwrap().unwrap();
        assert_eq!(stat.size, 3);
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let volume = LocalVolume::open(dir.path(), NodeId::new()).await.unwrap();
        let receipt = volume.put_shard(bytes_stream(vec![b"x"])).await.unwrap();

        volume.delete_shard(receipt.shard_id).await.unwrap();
        volume.delete_shard(receipt.shard_id).await.unwrap(); // no error on double delete
        assert_eq!(volume.stat_shard(receipt.shard_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_crash_leaves_no_referenceable_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate a crash between tmp-file creation and rename.
        tokio::fs::create_dir_all(dir.path().join("tmp"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("tmp").join("leftover.tmp"), b"partial")
            .await
            .unwrap();

        // Opening the volume (which happens on every process start) sweeps it away.
        let volume = LocalVolume::open(dir.path(), NodeId::new()).await.unwrap();
        let mut remaining = tokio::fs::read_dir(dir.path().join("tmp")).await.unwrap();
        assert!(remaining.next_entry().await.unwrap().is_none());

        drop(volume); // keep the volume alive for the duration of the assertions above
    }

    #[tokio::test]
    async fn manager_routes_by_target_and_rejects_unknown_or_remote_targets() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let node = NodeId::new();
        let manager = LocalVolumeManager::open(
            node,
            vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
        )
        .await
        .unwrap();

        let volume_ids: Vec<VolumeId> = manager.volume_ids().collect();
        assert_eq!(volume_ids.len(), 2);

        let good_target = ShardTarget {
            node_id: node,
            volume_id: volume_ids[0],
        };
        let receipt = manager
            .put_shard(good_target, Box::pin(bytes_stream(vec![b"data"])))
            .await
            .unwrap();
        let mut out = manager
            .get_shard(good_target, receipt.shard_id)
            .await
            .unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = out.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"data");

        let unknown_volume = ShardTarget {
            node_id: node,
            volume_id: VolumeId::new(),
        };
        let err = manager
            .put_shard(unknown_volume, Box::pin(bytes_stream(vec![b"x"])))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::UnknownVolume(_)));

        let remote_node = ShardTarget {
            node_id: NodeId::new(),
            volume_id: volume_ids[0],
        };
        let err = manager
            .put_shard(remote_node, Box::pin(bytes_stream(vec![b"x"])))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotLocal { .. }));
    }
}
