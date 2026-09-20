//! Cluster identity (architecture.md §36): a `CLUSTER_ID` file at the root of the data
//! directory, checked before adopting any cluster id a bootstrap/join operation would
//! set — a node that has already joined cluster A must refuse an operation that would
//! silently move it to cluster B, even across a restart.

use std::path::{Path, PathBuf};

use s3_core::ClusterId;

use crate::error::ClusterError;

pub struct ClusterIdentity;

impl ClusterIdentity {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("CLUSTER_ID")
    }

    pub async fn load(data_dir: &Path) -> Result<Option<ClusterId>, ClusterError> {
        match tokio::fs::read_to_string(Self::path(data_dir)).await {
            Ok(contents) => Ok(Some(ClusterId::parse(contents.trim().to_string())?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Checks any locally-persisted identity against `cluster_id`, refusing a mismatch
    /// (architecture.md §36), then persists it if this is the first time.
    pub async fn check_and_persist(
        data_dir: &Path,
        cluster_id: &ClusterId,
    ) -> Result<(), ClusterError> {
        if let Some(existing) = Self::load(data_dir).await? {
            if &existing != cluster_id {
                return Err(ClusterError::LocalClusterIdMismatch {
                    existing: existing.to_string(),
                    requested: cluster_id.to_string(),
                });
            }
            return Ok(());
        }

        tokio::fs::create_dir_all(data_dir).await?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, cluster_id.as_str().as_bytes()).await?;
        let file = tokio::fs::File::open(&tmp).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, &path).await?;
        if let Some(parent) = path.parent() {
            let dir = tokio::fs::File::open(parent).await?;
            dir.sync_all().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fresh_data_dir_has_no_identity() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ClusterIdentity::load(dir.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let id = ClusterId::parse("prod").unwrap();
        ClusterIdentity::check_and_persist(dir.path(), &id)
            .await
            .unwrap();
        assert_eq!(ClusterIdentity::load(dir.path()).await.unwrap(), Some(id));
    }

    #[tokio::test]
    async fn matching_id_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let id = ClusterId::parse("prod").unwrap();
        ClusterIdentity::check_and_persist(dir.path(), &id)
            .await
            .unwrap();
        ClusterIdentity::check_and_persist(dir.path(), &id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mismatched_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let prod = ClusterId::parse("prod").unwrap();
        ClusterIdentity::check_and_persist(dir.path(), &prod)
            .await
            .unwrap();

        let staging = ClusterId::parse("staging").unwrap();
        let err = ClusterIdentity::check_and_persist(dir.path(), &staging)
            .await
            .unwrap_err();
        assert!(matches!(err, ClusterError::LocalClusterIdMismatch { .. }));

        // The original identity is untouched by the rejected attempt.
        assert_eq!(ClusterIdentity::load(dir.path()).await.unwrap(), Some(prod));
    }
}
