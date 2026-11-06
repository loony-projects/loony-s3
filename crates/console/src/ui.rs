//! Serves the built web UI, for deployments with no other web server. It listens on its own
//! address because the LS3 API needs every path on its own: `/login` could be a bucket.

use std::path::Path;

use axum::Router;
use axum::http::{HeaderValue, Request, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

/// The UI keeps a signed-in person's keys in the page, so this is what limits an injected
/// script: only this site's own scripts run. It talks to the LS3 API and shows previews from
/// wherever that's hosted, which this deliberately doesn't need to be told.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
    style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
    font-src 'self' https://fonts.gstatic.com; img-src * data: blob:; media-src * blob:; frame-src *; \
    connect-src *; object-src 'none'; base-uri 'none'; form-action 'none'; \
    frame-ancestors 'none'";

/// File names under `assets/` change with their contents, so those can be kept forever.
/// Everything else must be checked, or a browser could hold on to an old version of the UI.
async fn cache_policy(request: Request<axum::body::Body>, next: Next) -> Response {
    let immutable = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    let value = if immutable && response.status().is_success() {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

/// Any path that isn't a file gets `index.html`, since the UI handles its own addresses. A
/// missing file under `assets/` stays a 404, so it isn't mistaken for a script.
pub fn ui_router(dir: &Path) -> Router {
    let index = dir.join("index.html");
    Router::new()
        .nest_service("/assets", ServeDir::new(dir.join("assets")))
        .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
        .layer(middleware::from_fn(cache_policy))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn site() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>the app</html>").unwrap();
        std::fs::write(dir.path().join("favicon.svg"), "<svg/>").unwrap();
        std::fs::write(dir.path().join("assets/app-abc123.js"), "let a = 1;").unwrap();
        std::fs::write(
            dir.path().parent().unwrap().join("loony-ui-outside.txt"),
            "secret",
        )
        .ok();
        dir
    }

    async fn get(dir: &Path, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = ui_router(dir).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn the_apps_own_addresses_get_the_app() {
        let site = site();
        for uri in [
            "/",
            "/login",
            "/callback?code=x&state=y",
            "/projects/abc/buckets/my-bucket",
        ] {
            let (status, headers, body) = get(site.path(), uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(body, "<html>the app</html>", "{uri}");
            assert_eq!(headers[header::CACHE_CONTROL], "no-cache", "{uri}");
        }
    }

    #[tokio::test]
    async fn files_are_served_and_hashed_assets_cached_for_good() {
        let site = site();
        let (status, headers, body) = get(site.path(), "/assets/app-abc123.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "let a = 1;");
        assert!(
            headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("immutable")
        );
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("javascript")
        );

        let (status, headers, body) = get(site.path(), "/favicon.svg").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<svg/>");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn a_missing_asset_is_not_answered_with_the_app() {
        let site = site();
        let (status, headers, body) = get(site.path(), "/assets/gone-old123.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_ne!(body, "<html>the app</html>");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn nothing_outside_the_directory_can_be_reached() {
        let site = site();
        for uri in [
            "/../loony-ui-outside.txt",
            "/assets/../../loony-ui-outside.txt",
            "/%2e%2e/loony-ui-outside.txt",
            "/assets/%2e%2e/%2e%2e/loony-ui-outside.txt",
        ] {
            let (_, _, body) = get(site.path(), uri).await;
            assert_ne!(body, "secret", "{uri}");
        }
    }

    #[tokio::test]
    async fn pages_carry_security_headers() {
        let site = site();
        let (_, headers, _) = get(site.path(), "/").await;
        let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(policy.contains("script-src \'self\'"));
        assert!(policy.contains("frame-ancestors \'none\'"));
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    }
}
