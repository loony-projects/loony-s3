//! Node identity (architecture.md §34): a `NODE_ID` file at the root of the data
//! directory, minted once and read back on every subsequent start. Never derived from
//! IP/hostname — a node keeps the same identity across restarts, address changes, and
//! (eventually) NIC/hostname changes.

use std::path::{Path, PathBuf};

use loony_core::{IdParseError, NodeId};

#[derive(Debug, thiserror::Error)]
pub enum NodeIdentityError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("NODE_ID file at {path} is corrupt: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: IdParseError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeIdentity {
    pub node_id: NodeId,
}

impl NodeIdentity {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("NODE_ID")
    }

    /// Load this node's persistent identity, minting and persisting a new one if
    /// `data_dir` is fresh. The same `data_dir` always yields the same `NodeId`.
    pub async fn load_or_create(data_dir: &Path) -> Result<Self, NodeIdentityError> {
        tokio::fs::create_dir_all(data_dir).await?;
        let path = Self::path(data_dir);

        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                let node_id = contents.trim().parse::<NodeId>().map_err(|source| {
                    NodeIdentityError::Corrupt {
                        path: path.clone(),
                        source,
                    }
                })?;
                Ok(Self { node_id })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let node_id = NodeId::new();
                write_atomic(&path, node_id.to_string().as_bytes()).await?;
                Ok(Self { node_id })
            }
            Err(e) => Err(e.into()),
        }
    }
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    let file = tokio::fs::File::open(&tmp).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    if let Some(parent) = path.parent() {
        let dir = tokio::fs::File::open(parent).await?;
        dir.sync_all().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mints_a_fresh_id_for_a_new_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::load_or_create(dir.path()).await.unwrap();
        assert!(dir.path().join("NODE_ID").exists());
        let _ = identity.node_id;
    }

    #[tokio::test]
    async fn survives_restart_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let first = NodeIdentity::load_or_create(dir.path()).await.unwrap();
        let second = NodeIdentity::load_or_create(dir.path()).await.unwrap();
        assert_eq!(first.node_id, second.node_id);
    }

    #[tokio::test]
    async fn two_different_data_dirs_never_collide() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = NodeIdentity::load_or_create(dir_a.path()).await.unwrap();
        let b = NodeIdentity::load_or_create(dir_b.path()).await.unwrap();
        assert_ne!(a.node_id, b.node_id);
    }

    #[tokio::test]
    async fn rejects_a_corrupt_node_id_file_instead_of_silently_regenerating() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("NODE_ID"), b"not-a-uuid")
            .await
            .unwrap();

        let err = NodeIdentity::load_or_create(dir.path()).await.unwrap_err();
        assert!(matches!(err, NodeIdentityError::Corrupt { .. }));
    }
}
