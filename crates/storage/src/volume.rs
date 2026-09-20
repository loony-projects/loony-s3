//! Volume identity (architecture.md §7/§48): each volume directory carries a
//! `VOLUME_META` file recording which node it belongs to, so a disk moved to the wrong
//! node is rejected loudly instead of silently adopted.

use std::path::{Path, PathBuf};

use s3_core::{NodeId, VolumeId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::atomic::write_atomic;
use crate::error::StorageError;

pub const VOLUME_META_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMeta {
    pub format_version: u32,
    pub volume_id: VolumeId,
    pub node_id: NodeId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl VolumeMeta {
    fn path(volume_root: &Path) -> PathBuf {
        volume_root.join("VOLUME_META")
    }

    /// Load this volume's existing metadata, or mint and persist fresh metadata for a
    /// brand-new volume directory. Errors (rather than silently repairing) if an
    /// existing volume's recorded `node_id` doesn't match `local_node`.
    pub async fn load_or_create(
        volume_root: &Path,
        local_node: NodeId,
    ) -> Result<Self, StorageError> {
        tokio::fs::create_dir_all(volume_root).await?;
        let path = Self::path(volume_root);

        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let meta: VolumeMeta = serde_json::from_slice(&bytes).map_err(|source| {
                    StorageError::CorruptVolumeMeta {
                        path: path.clone(),
                        source,
                    }
                })?;
                if meta.node_id != local_node {
                    return Err(StorageError::NodeMismatch {
                        path,
                        expected: meta.node_id,
                        actual: local_node,
                    });
                }
                Ok(meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let meta = VolumeMeta {
                    format_version: VOLUME_META_FORMAT_VERSION,
                    volume_id: VolumeId::new(),
                    node_id: local_node,
                    created_at: OffsetDateTime::now_utc(),
                };
                let bytes =
                    serde_json::to_vec_pretty(&meta).expect("VolumeMeta is always serializable");
                write_atomic(&path, &bytes).await?;
                Ok(meta)
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_fresh_metadata_for_a_new_volume_dir() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeId::new();

        let meta = VolumeMeta::load_or_create(dir.path(), node).await.unwrap();

        assert_eq!(meta.node_id, node);
        assert_eq!(meta.format_version, VOLUME_META_FORMAT_VERSION);
        assert!(dir.path().join("VOLUME_META").exists());
    }

    #[tokio::test]
    async fn reloading_returns_the_same_volume_id() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeId::new();

        let first = VolumeMeta::load_or_create(dir.path(), node).await.unwrap();
        let second = VolumeMeta::load_or_create(dir.path(), node).await.unwrap();

        assert_eq!(first.volume_id, second.volume_id);
    }

    #[tokio::test]
    async fn rejects_a_volume_that_belongs_to_a_different_node() {
        let dir = tempfile::tempdir().unwrap();
        let original_node = NodeId::new();
        VolumeMeta::load_or_create(dir.path(), original_node)
            .await
            .unwrap();

        let wrong_node = NodeId::new();
        let err = VolumeMeta::load_or_create(dir.path(), wrong_node)
            .await
            .unwrap_err();

        assert!(matches!(err, StorageError::NodeMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_a_corrupt_meta_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("VOLUME_META"), b"not json")
            .await
            .unwrap();

        let err = VolumeMeta::load_or_create(dir.path(), NodeId::new())
            .await
            .unwrap_err();

        assert!(matches!(err, StorageError::CorruptVolumeMeta { .. }));
    }
}
