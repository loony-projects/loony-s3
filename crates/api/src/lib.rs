//! Thin Axum HTTP layer: S3 REST routes, XML (de)serialization, S3 error-code mapping.
//! Delegates all business logic to `loony-object` (architecture.md §1) — handlers here do
//! request parsing and response shaping only.
//!
//! Phase 3/4 scope (prompt's own phase boundaries): CreateBucket, DeleteBucket,
//! HeadBucket, ListBuckets, PutObject, GetObject, HeadObject, DeleteObject,
//! ListObjectsV2, all behind SigV4 (header + presigned) authentication and
//! ownership-based authorization. Phase 10 adds CreateMultipartUpload/UploadPart/
//! ListParts/CompleteMultipartUpload/AbortMultipartUpload as query-param variants of
//! the same `/{bucket}/{key}` route (prompt §51). No Range requests yet, no versioning
//! query params yet — each is a later, separately-scoped phase.

mod auth;
mod error;
mod handlers;
mod http_date;
mod state;
mod xml;

use axum::Router;
use axum::routing::{get, put};
use http::HeaderName;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tower_http::request_id::{PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use loony_observability::UuidV7RequestId;

pub use state::AppState;

pub fn build_router(state: AppState) -> Router {
    let request_id_header = HeaderName::from_static("x-amz-request-id");

    Router::new()
        .route("/", get(handlers::bucket::list_buckets))
        .route(
            "/:bucket",
            put(handlers::bucket::create_bucket)
                .delete(handlers::bucket::delete_bucket)
                .head(handlers::bucket::head_bucket)
                .get(handlers::bucket::list_objects_v2),
        )
        .route(
            "/:bucket/*key",
            put(handlers::object::put_object)
                .get(handlers::object::get_object)
                .head(handlers::object::head_object)
                .delete(handlers::object::delete_object)
                .post(handlers::object::post_object),
        )
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(
                    request_id_header.clone(),
                    UuidV7RequestId,
                ))
                .layer(PropagateRequestIdLayer::new(request_id_header))
                .layer(TraceLayer::new_for_http())
                // Runs last, i.e. closest to the handlers: every route above requires a
                // valid SigV4 signature (prompt §52-53) before a handler ever sees the
                // request.
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth::sigv4_auth,
                )),
        )
        // Outermost layer: a browser-based client (prompt §71's frontend) is served
        // from a different origin than this API, so every request -- including the
        // preflight OPTIONS a browser sends ahead of any request carrying the SigV4
        // `authorization`/`x-amz-*` headers -- needs a CORS response. It has to wrap
        // the auth layer above, not sit inside it: preflight requests carry no
        // signature, so if `sigv4_auth` saw them first it would reject every one with
        // 403 and no real request would ever get past the browser's CORS check.
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers([
                    HeaderName::from_static("etag"),
                    HeaderName::from_static("x-amz-request-id"),
                ]),
        )
        .with_state(state)
}
