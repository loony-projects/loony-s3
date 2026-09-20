#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("bucket {0:?} does not exist")]
    NoSuchBucket(String),

    #[error("bucket {0:?} already exists")]
    BucketAlreadyExists(String),

    #[error("bucket {0:?} is not empty")]
    BucketNotEmpty(String),

    #[error("the specified key does not exist: {0:?}")]
    NoSuchKey(String),

    #[error("upload {0} does not exist")]
    NoSuchUpload(String),

    #[error("one or more of the specified parts could not be found")]
    InvalidPart,

    #[error("the list of parts was not in ascending order")]
    InvalidPartOrder,

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("background task panicked: {0}")]
    TaskPanicked(String),
}
