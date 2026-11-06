use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use loony_metadata::MetaError;
use serde::Serialize;

/// `code` is for the web UI to act on, and `message` is shown to the person as-is.
#[derive(Debug)]
pub struct ConsoleError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ConsoleError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn account_disabled() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "This account has been disabled. Contact the administrator.",
        )
    }
}

impl From<MetaError> for ConsoleError {
    fn from(err: MetaError) -> Self {
        if let MetaError::RaftUnavailable(reason) = &err {
            tracing::warn!(%reason, "console request couldn't reach the metadata leader");
            return Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "The server can't make that change right now. Try again in a moment.",
            );
        }
        tracing::error!(%err, "console API metadata error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Something went wrong on the server.",
        )
    }
}

#[derive(Serialize)]
struct Body<'a> {
    code: &'a str,
    message: &'a str,
}

impl IntoResponse for ConsoleError {
    fn into_response(self) -> Response {
        let body = Body {
            code: self.code,
            message: &self.message,
        };
        (self.status, Json(body)).into_response()
    }
}
