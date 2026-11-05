//! Every admin request must carry a valid SigV4 `Authorization` header from an enabled
//! credential with the `admin` flag. Rejections are written to the audit log.

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use loony_auth::{AuthError, RequestParts, verify};
use loony_metadata::MetadataStore;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::AdminState;
use crate::audit::AuditEntry;
use crate::error::AdminError;

const MAX_CLOCK_SKEW: Duration = Duration::minutes(15);
const MAX_BODY_BYTES: usize = 64 * 1024;

/// The access key that signed this request, for handlers to check and record.
#[derive(Clone)]
pub struct Actor(pub String);

pub async fn require_admin(
    State(state): State<AdminState>,
    request: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let target = parts.uri.path().to_string();
    let claimed = claimed_access_key(&parts.headers);

    let denied = async |status: StatusCode, reason: &str| {
        state
            .audit
            .record(AuditEntry {
                at: OffsetDateTime::now_utc(),
                actor: claimed.clone(),
                action: "auth".into(),
                target: target.clone(),
                outcome: "denied".into(),
                detail: Some(reason.to_string()),
            })
            .await;
        AdminError::new(status, reason).into_response()
    };

    // Presigned URLs are for sharing objects: an admin action shouldn't be something a
    // link can carry.
    if !parts.headers.contains_key(header::AUTHORIZATION) {
        return denied(StatusCode::UNAUTHORIZED, "Missing signature").await;
    }

    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    let verified = verify(
        &RequestParts {
            method: parts.method.as_str(),
            path: parts.uri.path(),
            query: parts.uri.query().unwrap_or(""),
            headers: &headers,
        },
        &state.region,
        state.metadata.as_ref(),
        OffsetDateTime::now_utc(),
        MAX_CLOCK_SKEW,
    )
    .await;
    let verified = match verified {
        Ok(verified) => verified,
        Err(AuthError::CredentialDisabled) => {
            return denied(StatusCode::FORBIDDEN, "This access key is disabled").await;
        }
        Err(err) => return denied(StatusCode::UNAUTHORIZED, &err.to_string()).await,
    };

    let is_admin = match state.metadata.get_credential(&verified.access_key).await {
        Ok(credential) => credential.is_some_and(|c| c.admin),
        Err(err) => return AdminError::from(err).into_response(),
    };
    if !is_admin {
        return denied(StatusCode::FORBIDDEN, "This access key is not an admin key").await;
    }

    // SigV4 only covers the body through this declared hash, so check it really matches.
    let Ok(bytes) = to_bytes(body, MAX_BODY_BYTES).await else {
        return denied(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").await;
    };
    let declared = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok());
    if declared != Some(sha256_hex(&bytes).as_str()) {
        return denied(StatusCode::BAD_REQUEST, "x-amz-content-sha256 does not match the body").await;
    }

    parts.extensions.insert(Actor(verified.access_key));
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Who a rejected request said it was. Unverified, so only ever used as a label.
fn claimed_access_key(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split("Credential=").nth(1))
        .and_then(|v| v.split('/').next())
        .map_or_else(|| "unknown".to_string(), |key| key.chars().take(128).collect())
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}
