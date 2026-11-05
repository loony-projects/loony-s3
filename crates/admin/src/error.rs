use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use loony_metadata::MetaError;
use serde::Serialize;

/// The message is shown to the operator as-is, so it should say what to do about it.
#[derive(Debug)]
pub struct AdminError {
    pub status: StatusCode,
    pub message: String,
}

impl AdminError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
}

impl From<MetaError> for AdminError {
    fn from(err: MetaError) -> Self {
        let status = match &err {
            MetaError::NoSuchBucket(_) | MetaError::NoSuchNode(_) => StatusCode::NOT_FOUND,
            MetaError::InvalidNodeTransition { .. } | MetaError::NodeRemoved(_) => {
                StatusCode::CONFLICT
            }
            MetaError::InvalidContinuationToken => StatusCode::BAD_REQUEST,
            MetaError::RaftUnavailable(_) => {
                return Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "This node can't make that change right now. Changes must go through the \
                         metadata leader's admin API. ({err})"
                    ),
                );
            }
            _ => {
                tracing::error!(%err, "admin API metadata error");
                return Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal metadata error");
            }
        };
        Self::new(status, err.to_string())
    }
}

#[derive(Serialize)]
struct Body<'a> {
    message: &'a str,
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status, Json(Body { message: &self.message })).into_response()
    }
}
