#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("cluster protocol error: {0}")]
    Rpc(#[from] s3_rpc::RpcClientError),
    #[error("metadata error: {0}")]
    Meta(#[from] s3_metadata::MetaError),
    #[error("invalid cluster id: {0}")]
    InvalidClusterId(#[from] s3_core::InvalidClusterId),
    #[error(
        "this node already belongs to cluster {existing:?} and cannot join/bootstrap cluster \
         {requested:?} — refusing to silently merge unrelated clusters"
    )]
    LocalClusterIdMismatch { existing: String, requested: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
