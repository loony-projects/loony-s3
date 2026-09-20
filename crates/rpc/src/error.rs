use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use s3_metadata::MetaError;
use s3_storage::StorageError;

/// Errors an RPC *server* handler can produce. Distinct from [`StorageError`]/
/// [`MetaError`] because "the caller didn't authenticate" or "this node hasn't
/// bootstrapped yet" aren't storage/metadata concerns in themselves — they're RPC-layer
/// conditions that happen to wrap those errors for the ones that pass through.
#[derive(Debug, thiserror::Error)]
pub enum RpcServerError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("this node has not bootstrapped or joined a cluster yet")]
    NotBootstrapped,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Meta(#[from] MetaError),
}

impl IntoResponse for RpcServerError {
    fn into_response(self) -> Response {
        match &self {
            RpcServerError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            RpcServerError::NotBootstrapped => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            RpcServerError::Storage(StorageError::NotFound(_)) => {
                StatusCode::NOT_FOUND.into_response()
            }
            RpcServerError::Storage(err) => {
                tracing::error!(error = %err, "internal RPC request failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
            RpcServerError::Meta(MetaError::ClusterIdMismatch { .. }) => {
                (StatusCode::CONFLICT, self.to_string()).into_response()
            }
            RpcServerError::Meta(MetaError::NoSuchNode(_)) => StatusCode::NOT_FOUND.into_response(),
            RpcServerError::Meta(err) => {
                tracing::error!(error = %err, "internal RPC request failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}
