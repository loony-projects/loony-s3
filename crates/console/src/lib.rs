//! The public API behind the web console: signing in with the identity provider, and
//! everything a signed-in user does that isn't an LS3 API call.
//!
//! Signing in swaps a one-time code from the identity provider for a short-lived session
//! key. The browser signs console requests with that key using SigV4, the same way it
//! signs LS3 API requests, so there are no cookies to protect. The key is refused by the
//! LS3 API itself. See docs/loony-auth-signin.md.

mod auth;
mod error;
mod handlers;
mod oidc;
mod projects;
mod rate_limit;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, Uri, header};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Router, middleware};
use loony_admin::AuditLog;
use loony_auth::CredentialProvider;
use loony_metadata::{MetaError, MetadataStore};
use time::OffsetDateTime;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

pub use oidc::OidcClient;
pub use projects::{MAX_ACCESS_KEYS_PER_PROJECT, MAX_PROJECTS_PER_USER};
pub use ui::ui_router;
use rate_limit::RateLimiter;

/// Far more than the sign-in request needs, which is a few short strings.
const MAX_BODY_BYTES: usize = 64 * 1024;

const SIGN_IN_ATTEMPTS_PER_MINUTE: u32 = 30;

const CLEANUP_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct ConsoleConfig {
    /// The identity provider's public URL, exactly as it appears in its tokens.
    pub issuer: String,
    /// Where this server reaches the identity provider, if not through `issuer`.
    pub internal_url: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    /// Where the identity provider may send a browser back to. Also decides which web
    /// origins may call the console API.
    pub redirect_uris: Vec<String>,
    /// How long a temporary key lasts.
    pub session_ttl: Duration,
    /// Header holding the real client address, set by a proxy all traffic goes through.
    pub client_ip_header: Option<String>,
}

/// Hand-written so the client secret can't end up in a log line.
impl std::fmt::Debug for ConsoleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleConfig")
            .field("issuer", &self.issuer)
            .field("internal_url", &self.internal_url)
            .field("client_id", &self.client_id)
            .field("redirect_uris", &self.redirect_uris)
            .field("session_ttl", &self.session_ttl)
            .field("client_ip_header", &self.client_ip_header)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ConsoleState {
    pub(crate) metadata: Arc<dyn MetadataStore>,
    pub(crate) credentials: Arc<dyn CredentialProvider>,
    pub(crate) region: String,
    pub(crate) oidc: Arc<OidcClient>,
    pub(crate) redirect_uris: Arc<Vec<String>>,
    pub(crate) session_ttl: time::Duration,
    pub(crate) client_ip_header: Option<String>,
    pub(crate) rate_limiter: Arc<RateLimiter>,
    pub(crate) audit: Arc<AuditLog>,
}

impl ConsoleState {
    pub fn new<M: MetadataStore + 'static>(
        config: ConsoleConfig,
        metadata: Arc<M>,
        region: String,
        audit: Arc<AuditLog>,
    ) -> Result<Self, reqwest::Error> {
        let session_ttl = time::Duration::try_from(config.session_ttl)
            .expect("the configured session lifetime is validated at startup");
        Ok(Self {
            metadata: metadata.clone(),
            credentials: metadata,
            region,
            oidc: Arc::new(OidcClient::new(&config)?),
            redirect_uris: Arc::new(config.redirect_uris),
            session_ttl,
            client_ip_header: config.client_ip_header,
            rate_limiter: Arc::new(RateLimiter::new(
                SIGN_IN_ATTEMPTS_PER_MINUTE,
                Duration::from_secs(60),
            )),
            audit,
        })
    }

    /// Replaces the sign-in limit. For tests.
    pub fn with_sign_in_limit(mut self, attempts: u32, window: Duration) -> Self {
        self.rate_limiter = Arc::new(RateLimiter::new(attempts, window));
        self
    }
}

/// The web origin of a URL, such as `https://example.com:8443`. `None` if it isn't an
/// absolute `http` or `https` URL.
pub fn origin_of(url: &str) -> Option<String> {
    let uri: Uri = url.parse().ok()?;
    let scheme = uri.scheme_str()?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    Some(format!("{scheme}://{}", uri.authority()?))
}

/// Whether a URL is `https`, or `http` to this machine. Anything else would send sign-in
/// codes or tokens across a network in the clear.
pub fn is_secure_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    match (uri.scheme_str(), uri.host()) {
        (Some("https"), Some(_)) => true,
        (Some("http"), Some(host)) => {
            matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        }
        _ => false,
    }
}

async fn no_store(mut response: Response) -> Response {
    // These responses hold keys and account details, and Cloudflare sits in front of the
    // live deployment.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub fn build_router(state: ConsoleState) -> Router {
    let origins: Vec<HeaderValue> = state
        .redirect_uris
        .iter()
        .filter_map(|uri| origin_of(uri))
        .filter_map(|origin| origin.parse().ok())
        .collect();

    use projects as p;
    let protected = Router::new()
        .route("/_console/v1/session", delete(handlers::delete_session))
        .route("/_console/v1/me", get(handlers::me))
        .route(
            "/_console/v1/projects",
            get(p::list_projects).post(p::create_project),
        )
        .route(
            "/_console/v1/projects/:project",
            get(p::get_project)
                .patch(p::rename_project)
                .delete(p::delete_project),
        )
        .route(
            "/_console/v1/projects/:project/session",
            post(p::open_project_session),
        )
        .route(
            "/_console/v1/projects/:project/leave",
            post(p::leave_project),
        )
        .route(
            "/_console/v1/projects/:project/keys",
            get(p::list_access_keys).post(p::create_access_key),
        )
        .route(
            "/_console/v1/projects/:project/keys/:access_key",
            patch(p::update_access_key).delete(p::delete_access_key),
        )
        .route(
            "/_console/v1/projects/:project/members",
            get(p::list_members),
        )
        .route(
            "/_console/v1/projects/:project/members/:user",
            patch(p::update_member).delete(p::remove_member),
        )
        .route(
            "/_console/v1/projects/:project/invites",
            get(p::list_invites).post(p::create_invite),
        )
        .route(
            "/_console/v1/projects/:project/invites/:invite",
            delete(p::cancel_invite),
        )
        .route(
            "/_console/v1/invites/:invite/accept",
            post(p::accept_invite),
        )
        .route(
            "/_console/v1/invites/:invite/decline",
            post(p::decline_invite),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    Router::new()
        .route("/_console/v1/session", post(handlers::create_session))
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::map_response(no_store))
        // Only the console's own pages may call this API. The LS3 API's CORS policy is
        // separate and stays open, since a presigned link can be used from anywhere.
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
                .allow_headers(AllowHeaders::mirror_request()),
        )
        .with_state(state)
}

/// Deletes temporary keys and invites past their expiry. They already don't work by then:
/// this only stops them piling up. Reads first so a quiet server writes nothing.
pub async fn delete_expired_credentials(
    metadata: &dyn MetadataStore,
    now: OffsetDateTime,
) -> Result<usize, MetaError> {
    let mut deleted = 0;
    for credential in metadata.list_credentials().await? {
        if credential.is_expired(now) {
            metadata.delete_credential(&credential.access_key).await?;
            deleted += 1;
        }
    }
    for invite in metadata.list_invites().await? {
        if invite.expires_at <= now {
            metadata.delete_invite(invite.invite_id).await?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

pub async fn run_cleanup_loop(metadata: Arc<dyn MetadataStore>) {
    let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
    loop {
        interval.tick().await;
        match delete_expired_credentials(metadata.as_ref(), OffsetDateTime::now_utc()).await {
            Ok(0) => {}
            Ok(deleted) => tracing::info!(deleted, "deleted expired keys and invites"),
            // Only the metadata leader can write, so other nodes end up here.
            Err(MetaError::RaftUnavailable(reason)) => {
                tracing::debug!(%reason, "left expired keys for the metadata leader");
            }
            Err(err) => tracing::warn!(%err, "failed to delete expired keys and invites"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_keep_the_scheme_host_and_port_only() {
        assert_eq!(
            origin_of("https://s3-console.example.com/callback?x=1").as_deref(),
            Some("https://s3-console.example.com")
        );
        assert_eq!(
            origin_of("http://localhost:5173/callback").as_deref(),
            Some("http://localhost:5173")
        );
        for url in ["", "/callback", "ftp://example.com/x", "not a url"] {
            assert_eq!(origin_of(url), None, "{url:?}");
        }
    }

    #[test]
    fn only_https_or_local_http_counts_as_secure() {
        for url in [
            "https://auth.example.com",
            "http://localhost:5173/callback",
            "http://127.0.0.1:2001",
            "http://[::1]:2001",
        ] {
            assert!(is_secure_url(url), "{url}");
        }
        for url in [
            "http://auth.example.com",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "ftp://localhost/x",
            "/callback",
            "",
        ] {
            assert!(!is_secure_url(url), "{url:?}");
        }
    }

    #[test]
    fn debug_output_leaves_out_the_client_secret() {
        let config = ConsoleConfig {
            issuer: "https://issuer.example".into(),
            internal_url: None,
            client_id: "client".into(),
            client_secret: "do-not-print".into(),
            redirect_uris: vec![],
            session_ttl: Duration::from_secs(60),
            client_ip_header: None,
        };
        assert!(!format!("{config:?}").contains("do-not-print"));
    }
}
