//! SigV4 middleware: every request must carry a valid `Authorization` header or
//! presigned-URL signature before it reaches a handler (prompt §52-53). On success, the
//! resolved owner is inserted into the request's extensions as [`AuthenticatedOwner`]
//! for handlers to use; `loony-object`'s ownership checks are the authorization half
//! (architecture.md §55) — this middleware only answers "who are you?".

use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use time::{Duration, OffsetDateTime};

use loony_auth::{RequestParts, verify};
use loony_core::OwnerId;

use crate::error::auth_error_response;
use crate::handlers::request_id;
use crate::state::AppState;

/// Clock skew tolerance either side of "now" (prompt §52's `x-amz-date` requirement).
/// AWS's own SDKs default to a similar window.
const MAX_CLOCK_SKEW: Duration = Duration::minutes(15);

#[derive(Clone, Copy)]
pub struct AuthenticatedOwner(pub OwnerId);

pub async fn sigv4_auth(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let rid = request_id(request.headers());
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();

    let parts = RequestParts {
        method: &method,
        path: &path,
        query: &query,
        headers: &headers,
    };

    match verify(
        &parts,
        &state.region,
        state.credentials.as_ref(),
        OffsetDateTime::now_utc(),
        MAX_CLOCK_SKEW,
    )
    .await
    {
        Ok(verified) => {
            request
                .extensions_mut()
                .insert(AuthenticatedOwner(verified.owner_id));
            next.run(request).await
        }
        Err(err) => auth_error_response(err, rid),
    }
}
