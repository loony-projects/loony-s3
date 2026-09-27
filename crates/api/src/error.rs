use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use loony_auth::AuthError;
use loony_object::Ls3Error;

use crate::xml;

/// Wraps [`Ls3Error`] with the per-request context (request id, resource path) needed to
/// build a compliant `<Error>` body (prompt §57) — never leaking internal details:
/// [`Ls3Error::Meta`]/[`Ls3Error::Storage`] always render as a generic `InternalError`,
/// with the real error only going to the trace log.
pub struct ApiError {
    error: Ls3Error,
    request_id: String,
    resource: Option<String>,
}

impl ApiError {
    pub fn new(error: Ls3Error, request_id: String, resource: Option<String>) -> Self {
        Self {
            error,
            request_id,
            resource,
        }
    }
}

fn map_error(error: &Ls3Error) -> (StatusCode, &'static str, String) {
    match error {
        Ls3Error::NoSuchBucket => (
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "The specified bucket does not exist".into(),
        ),
        Ls3Error::NoSuchKey => (
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist".into(),
        ),
        Ls3Error::BucketAlreadyExists => (
            StatusCode::CONFLICT,
            "BucketAlreadyExists",
            "The requested bucket name is not available".into(),
        ),
        Ls3Error::BucketNotEmpty => (
            StatusCode::CONFLICT,
            "BucketNotEmpty",
            "The bucket you tried to delete is not empty".into(),
        ),
        Ls3Error::InvalidBucketName(reason) => {
            (StatusCode::BAD_REQUEST, "InvalidBucketName", reason.clone())
        }
        Ls3Error::InvalidArgument(reason) => {
            (StatusCode::BAD_REQUEST, "InvalidArgument", reason.clone())
        }
        Ls3Error::AccessDenied => (
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied".to_string(),
        ),
        Ls3Error::NoSuchUpload => (
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "The specified multipart upload does not exist".into(),
        ),
        Ls3Error::InvalidPart => (
            StatusCode::BAD_REQUEST,
            "InvalidPart",
            "One or more of the specified parts could not be found".into(),
        ),
        Ls3Error::InvalidPartOrder => (
            StatusCode::BAD_REQUEST,
            "InvalidPartOrder",
            "The list of parts was not in ascending order".into(),
        ),
        Ls3Error::Meta(_) | Ls3Error::Storage(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            "We encountered an internal error. Please try again.".into(),
        ),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.error.is_internal() {
            tracing::error!(error = %self.error, request_id = %self.request_id, "internal error handling LS3 request");
        }
        let (status, code, message) = map_error(&self.error);
        let body = xml::error_xml(code, &message, &self.request_id, self.resource.as_deref());
        (status, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
    }
}

/// Maps a SigV4 verification failure straight to a response — used by the auth
/// middleware, which runs before any handler and so has no [`Ls3Error`] to wrap. Codes
/// mirror the upstream protocol's own vocabulary for these cases (prompt §57:
/// `SignatureDoesNotMatch`, `InvalidAccessKeyId`).
pub fn auth_error_response(error: AuthError, request_id: String) -> Response {
    let (status, code, message): (StatusCode, &'static str, String) = match error {
        AuthError::MissingSignature => (
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Request is missing required authentication".into(),
        ),
        AuthError::Malformed | AuthError::MissingHeader(_) | AuthError::MissingQueryParam(_) => (
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            error.to_string(),
        ),
        AuthError::UnsupportedAlgorithm => (
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "Unsupported signing algorithm".into(),
        ),
        AuthError::RequestExpired => (
            StatusCode::FORBIDDEN,
            "RequestTimeTooSkewed",
            "The difference between the request time and the current time is too large".into(),
        ),
        AuthError::UnknownAccessKey => (
            StatusCode::FORBIDDEN,
            "InvalidAccessKeyId",
            "The access key ID you provided does not exist in our records".into(),
        ),
        AuthError::CredentialDisabled => (
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "This credential is disabled".into(),
        ),
        AuthError::SignatureMismatch | AuthError::InvalidCredentialScope => (
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided".into(),
        ),
    };
    let body = xml::error_xml(code, &message, &request_id, None);
    (status, [(header::CONTENT_TYPE, "application/xml")], body).into_response()
}
