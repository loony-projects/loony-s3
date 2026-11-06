//! Console requests after sign-in must carry a valid SigV4 `Authorization` header from a
//! session key belonging to a user who is still enabled.

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use loony_auth::{AuthError, RequestParts, verify};
use loony_metadata::{CredentialKind, User};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::error::ConsoleError;
use crate::{ConsoleState, MAX_BODY_BYTES};

const MAX_CLOCK_SKEW: Duration = Duration::minutes(15);

/// Who a signed-in request is from.
#[derive(Clone)]
pub struct SignedIn {
    pub access_key: String,
    pub user: User,
}

fn unauthorized(message: &str) -> Response {
    ConsoleError::new(StatusCode::UNAUTHORIZED, "unauthorized", message).into_response()
}

pub async fn require_session(
    State(state): State<ConsoleState>,
    request: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();

    // Presigned URLs are for sharing objects: a console action shouldn't be something a
    // link can carry.
    if !parts.headers.contains_key(header::AUTHORIZATION) {
        return unauthorized("Sign in to continue.");
    }

    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let verified = verify(
        &RequestParts {
            method: parts.method.as_str(),
            path: parts.uri.path(),
            query: parts.uri.query().unwrap_or(""),
            headers: &headers,
        },
        &state.region,
        state.credentials.as_ref(),
        OffsetDateTime::now_utc(),
        MAX_CLOCK_SKEW,
    )
    .await;
    let verified = match verified {
        Ok(verified) => verified,
        // The web UI's cue to send the person back through sign-in.
        Err(AuthError::CredentialExpired) => {
            return ConsoleError::new(
                StatusCode::UNAUTHORIZED,
                "session_expired",
                "Your session has expired. Sign in again.",
            )
            .into_response();
        }
        Err(_) => return unauthorized("Sign in to continue."),
    };

    if verified.kind != CredentialKind::UserSession {
        return ConsoleError::new(
            StatusCode::FORBIDDEN,
            "wrong_key",
            "This key can't be used to sign in to the console.",
        )
        .into_response();
    }
    let Some(user_id) = verified.user_id else {
        return unauthorized("Sign in to continue.");
    };

    let user = match state.metadata.get_user(user_id).await {
        Ok(user) => user,
        Err(err) => return ConsoleError::from(err).into_response(),
    };
    // Checked on every request, so disabling someone takes effect immediately.
    let user = match user {
        Some(user) if user.enabled => user,
        _ => return ConsoleError::account_disabled().into_response(),
    };

    // SigV4 only covers the body through this declared hash, so check it really matches.
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return ConsoleError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request",
            "The request body is too large.",
        )
        .into_response();
    };
    let declared = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok());
    if declared != Some(sha256_hex(&bytes).as_str()) {
        return ConsoleError::invalid_request("x-amz-content-sha256 does not match the body")
            .into_response();
    }

    parts.extensions.insert(SignedIn {
        access_key: verified.access_key,
        user,
    });
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
