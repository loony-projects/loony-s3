use s3_metadata::MetaError;
use s3_storage::StorageError;

/// Unified error type for the Object Service. Deliberately mirrors S3's own error
/// vocabulary (prompt §57) rather than leaking `MetaError`/`StorageError` variants
/// directly, so `s3-api` can map each one to the right HTTP status + XML code without
/// string-matching messages.
#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("the specified bucket does not exist")]
    NoSuchBucket,
    #[error("the specified key does not exist")]
    NoSuchKey,
    #[error("the requested bucket name is not available")]
    BucketAlreadyExists,
    #[error("the bucket you tried to delete is not empty")]
    BucketNotEmpty,
    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("access denied")]
    AccessDenied,

    #[error("metadata store error: {0}")]
    Meta(#[from] MetaError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl S3Error {
    /// Whether this error reflects an internal fault (never safe to expose details of
    /// to the client, per prompt §57) as opposed to a well-defined S3-level condition.
    pub fn is_internal(&self) -> bool {
        matches!(self, S3Error::Meta(_) | S3Error::Storage(_))
    }
}
