use loony_metadata::MetaError;
use loony_storage::StorageError;

/// Unified error type for the Object Service. Deliberately mirrors the upstream protocol's own error
/// vocabulary (prompt §57) rather than leaking `MetaError`/`StorageError` variants
/// directly, so `loony-api` can map each one to the right HTTP status + XML code without
/// string-matching messages.
#[derive(Debug, thiserror::Error)]
pub enum Ls3Error {
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
    #[error("the specified multipart upload does not exist")]
    NoSuchUpload,
    #[error("one or more of the specified parts could not be found")]
    InvalidPart,
    #[error("the list of parts was not in ascending order")]
    InvalidPartOrder,
    #[error("the request body does not match its declared checksum: {0}")]
    BadDigest(String),
    #[error("the request body is incomplete or malformed: {0}")]
    IncompleteBody(String),
    #[error("a chunk signature in the request body does not match")]
    SignatureDoesNotMatch,
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("the requested range is not satisfiable for an object of {size} bytes")]
    InvalidRange { size: u64 },
    #[error("a precondition on the request did not hold")]
    PreconditionFailed,

    #[error("{0}")]
    QuotaExceeded(String),
    #[error("{0}")]
    ProjectReadOnly(String),

    #[error("metadata store error: {0}")]
    Meta(MetaError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl Ls3Error {
    /// Whether this error reflects an internal fault (never safe to expose details of
    /// to the client, per prompt §57) as opposed to a well-defined LS3-level condition.
    pub fn is_internal(&self) -> bool {
        matches!(self, Ls3Error::Meta(_) | Ls3Error::Storage(_))
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A project's subscription and storage limit become errors an S3 client can read, rather
/// than an internal error.
impl From<MetaError> for Ls3Error {
    fn from(err: MetaError) -> Self {
        use loony_metadata::SubscriptionState as State;
        match err {
            MetaError::QuotaExceeded {
                limit,
                used,
                requested,
            } => Ls3Error::QuotaExceeded(format!(
                "This would take the project over its plan's storage: {} of {} used, {} more \
                 needed. Delete files to make room.",
                bytes(used),
                bytes(limit),
                bytes(requested)
            )),
            MetaError::ProjectNotWritable(state) => Ls3Error::ProjectReadOnly(
                match state {
                    State::Unpaid => {
                        "This project has no subscription yet. Subscribe to it in \
                                      the console to store data."
                    }
                    State::Suspended => {
                        "This project is suspended because its subscription \
                                         lapsed. Renew it in the console."
                    }
                    _ => {
                        "This project is read-only because its subscription lapsed or was \
                          cancelled. Renew it in the console to make changes."
                    }
                }
                .to_string(),
            ),
            MetaError::ProjectDisabled(_) => Ls3Error::AccessDenied,
            other => Ls3Error::Meta(other),
        }
    }
}
