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
        let conflict = |code, message: &str| Self::new(StatusCode::CONFLICT, code, message);
        match &err {
            // Same as for a non-member: don't confirm that any of these exist. Being invited
            // at another address reads the same as there being no invite.
            MetaError::NoSuchProject(_)
            | MetaError::NoSuchMember
            | MetaError::NoSuchInvite
            | MetaError::NoSuchCredential(_)
            | MetaError::InviteEmailMismatch => {
                return Self::new(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "That doesn't exist, or you don't have access to it.",
                );
            }
            MetaError::ProjectDisabled(_) => {
                return Self::new(
                    StatusCode::FORBIDDEN,
                    "project_disabled",
                    "An administrator has disabled this project.",
                );
            }
            MetaError::EmailNotVerified => {
                return Self::new(
                    StatusCode::FORBIDDEN,
                    "email_not_verified",
                    "Verify your email address before accepting invites.",
                );
            }
            MetaError::ProjectNotEmpty(_) => {
                return conflict(
                    "project_not_empty",
                    "Delete this project's buckets before deleting the project.",
                );
            }
            MetaError::ProjectLimitReached => {
                return conflict(
                    "project_limit_reached",
                    "You've created as many projects as allowed.",
                );
            }
            MetaError::KeyLimitReached => {
                return conflict(
                    "key_limit_reached",
                    "This project has as many access keys as allowed. Delete one first.",
                );
            }
            MetaError::LastOwner => {
                return conflict(
                    "last_owner",
                    "A project needs at least one Owner. Make someone else an Owner first.",
                );
            }
            MetaError::InviteExpired => {
                return conflict(
                    "invite_expired",
                    "That invite has expired. Ask for a new one.",
                );
            }
            MetaError::InviteExists => {
                return conflict(
                    "invite_exists",
                    "That email address already has a pending invite to this project.",
                );
            }
            MetaError::AlreadyMember => {
                return conflict(
                    "already_member",
                    "That person is already a member of this project.",
                );
            }
            _ => {}
        }
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
