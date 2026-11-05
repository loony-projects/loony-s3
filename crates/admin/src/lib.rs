//! The admin API behind the frontend's admin panel: JSON over HTTP on its own port,
//! separate from the public LS3 API and restricted to loopback by the server's config.
//! Requests are SigV4-signed by a credential whose `admin` flag is set. See
//! docs/admin.md for what each operation does and doesn't do.

mod audit;
mod auth;
mod error;
mod handlers;

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, Method};
use axum::routing::{get, post};
use loony_core::NodeId;
use loony_metadata::RaftMetadataStore;
use time::OffsetDateTime;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

pub use audit::{AuditEntry, AuditLog};

#[derive(Clone)]
pub struct AdminState {
    pub metadata: Arc<RaftMetadataStore>,
    pub audit: Arc<AuditLog>,
    pub region: String,
    pub node_id: NodeId,
    pub mode: &'static str,
    pub started_at: OffsetDateTime,
}

pub fn build_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/v1/overview", get(handlers::overview))
        .route("/admin/v1/nodes", get(handlers::list_nodes))
        .route("/admin/v1/nodes/:node_id/drain", post(handlers::drain_node))
        .route("/admin/v1/nodes/:node_id/resume", post(handlers::resume_node))
        .route("/admin/v1/nodes/:node_id/remove", post(handlers::remove_node))
        .route("/admin/v1/buckets", get(handlers::list_buckets))
        .route("/admin/v1/buckets/:name/objects", get(handlers::list_objects))
        .route(
            "/admin/v1/credentials",
            get(handlers::list_credentials).post(handlers::create_credential),
        )
        .route(
            "/admin/v1/credentials/:access_key",
            axum::routing::patch(handlers::set_credential_enabled)
                .delete(handlers::delete_credential),
        )
        .route("/admin/v1/audit", get(handlers::list_audit))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ))
        // Outermost, because preflights carry no signature. A page on any other origin
        // (including the public site) has no business talking to this port.
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin, _| is_local_origin(origin)))
                .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
                .allow_headers(Any),
        )
        .with_state(state)
}

fn is_local_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = if authority.starts_with('[') {
        authority.split_once(']').map_or(authority, |(h, _)| h).trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_origins_are_local() {
        for origin in ["http://localhost:5173", "http://127.0.0.1:4173", "http://[::1]:5173"] {
            assert!(is_local_origin(&HeaderValue::from_static(origin)), "{origin}");
        }
        for origin in [
            "https://s3.lovegood.in",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example:5173",
            "null",
        ] {
            assert!(!is_local_origin(&HeaderValue::from_static(origin)), "{origin}");
        }
    }
}
