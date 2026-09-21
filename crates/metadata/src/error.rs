/// Clone + Serialize/Deserialize so a business-logic failure (e.g. `BucketAlreadyExists`)
/// can travel back through `openraft`'s `client_write` response channel as `C::R`
/// (Phase 8) exactly like a successful result does — not just infra-level failures like
/// "not the leader", which `openraft` reports separately via `RaftError`.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
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

    #[error("node {0} is not registered in this cluster")]
    NoSuchNode(String),

    #[error(
        "this node already belongs to cluster {existing:?} and cannot join/bootstrap cluster {requested:?} \
         — refusing to silently merge unrelated clusters"
    )]
    ClusterIdMismatch { existing: String, requested: String },

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("background task panicked: {0}")]
    TaskPanicked(String),

    /// This node's Raft engine couldn't service the request — it isn't the leader (and
    /// doesn't know who is), lost quorum, or the write timed out waiting for consensus
    /// (Phase 8). Distinct from every other variant here, which are business-logic
    /// outcomes a *successful* Raft commit can still produce.
    #[error("metadata Raft group unavailable: {0}")]
    RaftUnavailable(String),
}

impl From<serde_json::Error> for MetaError {
    fn from(e: serde_json::Error) -> Self {
        MetaError::Serialization(e.to_string())
    }
}
