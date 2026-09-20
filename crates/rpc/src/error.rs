use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use s3_storage::StorageError;

/// Errors an RPC *server* handler can produce. Distinct from [`StorageError`] because
/// "the caller didn't authenticate" isn't a storage concern — it's turned into a
/// `StorageError::Remote`/`Unreachable` variant on the *client* side instead, once it
/// crosses back into the `ShardStore` trait's error type.
#[derive(Debug, thiserror::Error)]
pub enum RpcServerError {
    #[error("unauthorized")]
    Unauthorized,
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl IntoResponse for RpcServerError {
    fn into_response(self) -> Response {
        match &self {
            RpcServerError::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            RpcServerError::Storage(StorageError::NotFound(_)) => {
                StatusCode::NOT_FOUND.into_response()
            }
            RpcServerError::Storage(err) => {
                tracing::error!(error = %err, "internal RPC request failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}
