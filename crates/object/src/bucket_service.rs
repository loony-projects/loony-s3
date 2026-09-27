use std::sync::Arc;

use loony_core::{Bucket, BucketName, OwnerId};
use loony_metadata::{CreateBucket, MetaError, MetadataStore};

use crate::error::Ls3Error;

/// The Bucket Service (architecture.md §1/§5): CreateBucket/DeleteBucket/HeadBucket/
/// ListBuckets, translating [`MetaError`] into the LS3-shaped [`Ls3Error`] the API layer
/// expects, and enforcing ownership-based authorization (architecture.md §55: initial
/// release is ownership/root-style — a request may only act on a bucket it owns).
/// Contains no HTTP concerns — `loony-api`'s handlers are the only thing that knows this
/// exists.
pub struct BucketService {
    metadata: Arc<dyn MetadataStore>,
}

impl BucketService {
    pub fn new(metadata: Arc<dyn MetadataStore>) -> Self {
        Self { metadata }
    }

    pub async fn create_bucket(
        &self,
        name: BucketName,
        owner_id: OwnerId,
        region: String,
    ) -> Result<Bucket, Ls3Error> {
        match self
            .metadata
            .create_bucket(CreateBucket {
                name,
                owner_id,
                region,
            })
            .await
        {
            Err(MetaError::BucketAlreadyExists(_)) => Err(Ls3Error::BucketAlreadyExists),
            other => Ok(other?),
        }
    }

    /// Fetches the bucket, failing with `NoSuchBucket` if it doesn't exist or
    /// `AccessDenied` if it exists but isn't owned by `requesting_owner`.
    pub async fn head_bucket(
        &self,
        name: &BucketName,
        requesting_owner: OwnerId,
    ) -> Result<Bucket, Ls3Error> {
        let bucket = self
            .metadata
            .get_bucket(name)
            .await?
            .ok_or(Ls3Error::NoSuchBucket)?;
        if bucket.owner_id != requesting_owner {
            return Err(Ls3Error::AccessDenied);
        }
        Ok(bucket)
    }

    pub async fn delete_bucket(
        &self,
        name: &BucketName,
        requesting_owner: OwnerId,
    ) -> Result<(), Ls3Error> {
        self.head_bucket(name, requesting_owner).await?;
        match self.metadata.delete_bucket(name).await {
            Err(MetaError::NoSuchBucket(_)) => Err(Ls3Error::NoSuchBucket),
            Err(MetaError::BucketNotEmpty(_)) => Err(Ls3Error::BucketNotEmpty),
            other => Ok(other?),
        }
    }

    /// Unlike the others, this one has no bucket to check ownership against — the
    /// owner filter *is* the authorization (a caller only ever sees their own
    /// buckets).
    pub async fn list_buckets(&self, owner: OwnerId) -> Result<Vec<Bucket>, Ls3Error> {
        Ok(self.metadata.list_buckets(owner).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loony_metadata::RedbMetadataStore;

    async fn service() -> BucketService {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbMetadataStore::open(dir.keep().join("meta.redb"))
            .await
            .unwrap();
        BucketService::new(Arc::new(store))
    }

    #[tokio::test]
    async fn create_head_list_delete_roundtrip() {
        let service = service().await;
        let owner = OwnerId::new();
        let name = BucketName::parse("bucket-one").unwrap();

        let created = service
            .create_bucket(name.clone(), owner, "us-east-1".into())
            .await
            .unwrap();
        let headed = service.head_bucket(&name, owner).await.unwrap();
        assert_eq!(created.bucket_id, headed.bucket_id);

        let listed = service.list_buckets(owner).await.unwrap();
        assert_eq!(listed.len(), 1);

        service.delete_bucket(&name, owner).await.unwrap();
        let err = service.head_bucket(&name, owner).await.unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchBucket));
    }

    #[tokio::test]
    async fn duplicate_create_is_rejected() {
        let service = service().await;
        let name = BucketName::parse("dup").unwrap();
        service
            .create_bucket(name.clone(), OwnerId::new(), "us-east-1".into())
            .await
            .unwrap();
        let err = service
            .create_bucket(name, OwnerId::new(), "us-east-1".into())
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::BucketAlreadyExists));
    }

    #[tokio::test]
    async fn a_different_owner_cannot_head_or_delete_the_bucket() {
        let service = service().await;
        let owner = OwnerId::new();
        let other = OwnerId::new();
        let name = BucketName::parse("owned-bucket").unwrap();
        service
            .create_bucket(name.clone(), owner, "us-east-1".into())
            .await
            .unwrap();

        let err = service.head_bucket(&name, other).await.unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));

        let err = service.delete_bucket(&name, other).await.unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));

        // The bucket is still there -- the denied delete had no effect.
        service.head_bucket(&name, owner).await.unwrap();
    }
}
