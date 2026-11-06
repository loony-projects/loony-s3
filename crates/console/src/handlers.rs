use std::net::SocketAddr;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use loony_core::OwnerId;
use loony_metadata::{Credential, CredentialKind, User, UserSignIn};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ConsoleState;
use crate::auth::SignedIn;
use crate::error::ConsoleError;
use crate::oidc::OidcError;

type Result<T> = std::result::Result<T, ConsoleError>;

#[derive(Serialize)]
pub struct UserView {
    id: String,
    email: Option<String>,
    email_verified: bool,
    name: Option<String>,
    can_create_projects: bool,
}

impl From<User> for UserView {
    fn from(user: User) -> Self {
        Self {
            id: user.user_id.to_string(),
            email: user.email,
            email_verified: user.email_verified,
            name: user.name,
            can_create_projects: user.can_create_projects,
        }
    }
}

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    code: String,
    code_verifier: String,
    redirect_uri: String,
    nonce: String,
}

#[derive(Serialize)]
pub struct CreatedSession {
    access_key: String,
    secret_key: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
    user: UserView,
}

/// `Uuid::new_v4` draws from the OS CSPRNG, for 122 random bits per id.
fn random_hex(ids: usize) -> String {
    (0..ids)
        .map(|_| uuid::Uuid::new_v4().simple().to_string())
        .collect()
}

fn client_key(
    state: &ConsoleState,
    headers: &HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> String {
    state
        .client_ip_header
        .as_deref()
        .and_then(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| peer.map(|ConnectInfo(addr)| addr.ip().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

pub async fn create_session(
    State(state): State<ConsoleState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    request: std::result::Result<Json<CreateSessionRequest>, JsonRejection>,
) -> Result<Response> {
    if !state
        .rate_limiter
        .allow(&client_key(&state, &headers, peer))
    {
        let error = ConsoleError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_requests",
            "Too many sign-in attempts. Wait a minute and try again.",
        );
        return Ok(([(header::RETRY_AFTER, "60")], error).into_response());
    }

    let Json(request) =
        request.map_err(|_| ConsoleError::invalid_request("The sign-in request is malformed."))?;
    if request.code.is_empty() || request.code_verifier.is_empty() || request.nonce.is_empty() {
        return Err(ConsoleError::invalid_request(
            "The sign-in request is missing something it needs.",
        ));
    }
    // Compared here as well as by the provider, so a bad address never gets that far.
    if !state.redirect_uris.contains(&request.redirect_uri) {
        return Err(ConsoleError::invalid_request(
            "That sign-in return address isn't allowed.",
        ));
    }

    let claims = state
        .oidc
        .sign_in(
            &request.code,
            &request.code_verifier,
            &request.redirect_uri,
            &request.nonce,
        )
        .await
        .map_err(|err| match err {
            OidcError::Rejected(reason) => {
                tracing::info!(reason, "refused a sign-in");
                ConsoleError::new(
                    StatusCode::UNAUTHORIZED,
                    "sign_in_failed",
                    "Sign-in failed. Please try again.",
                )
            }
            OidcError::Unavailable => ConsoleError::new(
                StatusCode::BAD_GATEWAY,
                "identity_provider_unavailable",
                "The sign-in service is unavailable right now. Try again in a moment.",
            ),
        })?;

    let user = state
        .metadata
        .record_user_sign_in(UserSignIn {
            issuer: claims.iss,
            subject: claims.sub,
            email: claims.email,
            email_verified: claims.email_verified.unwrap_or(false),
            name: claims.name,
        })
        .await?;
    if !user.enabled {
        return Err(ConsoleError::account_disabled());
    }

    let now = OffsetDateTime::now_utc();
    let expires_at = now + state.session_ttl;
    let credential = Credential {
        access_key: format!("LS3S{}", random_hex(1)[..16].to_uppercase()),
        secret_key: random_hex(2),
        // A session owns nothing, and the LS3 API refuses this kind of key anyway.
        owner_id: OwnerId::new(),
        enabled: true,
        admin: false,
        kind: CredentialKind::UserSession,
        user_id: Some(user.user_id),
        expires_at: Some(expires_at),
        created_at: now,
    };
    state.metadata.put_credential(credential.clone()).await?;

    Ok(Json(CreatedSession {
        access_key: credential.access_key,
        secret_key: credential.secret_key,
        expires_at,
        user: user.into(),
    })
    .into_response())
}

/// Signing out ends this session's key straight away, rather than leaving it to expire.
pub async fn delete_session(
    State(state): State<ConsoleState>,
    Extension(signed_in): Extension<SignedIn>,
) -> Result<StatusCode> {
    state
        .metadata
        .delete_credential(&signed_in.access_key)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(Extension(signed_in): Extension<SignedIn>) -> Json<UserView> {
    Json(signed_in.user.into())
}
