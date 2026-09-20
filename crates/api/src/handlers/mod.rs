pub mod bucket;
pub mod object;

use axum::http::HeaderMap;
use uuid::Uuid;

/// `SetRequestIdLayer` (wired in `lib.rs`) puts the request id in this header before
/// any handler runs; falling back to minting one covers direct unit-testing of a
/// handler without the middleware stack applied.
pub(crate) fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-amz-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::now_v7().to_string())
}
