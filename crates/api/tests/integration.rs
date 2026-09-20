//! End-to-end tests against the real Axum router (no mocked handlers) — the same
//! `Router` `s3-server` mounts, driven with `tower::ServiceExt::oneshot` rather than a
//! bound TCP socket so the tests stay fast and hermetic. Every request is signed with
//! real SigV4 (via `s3_auth::sign_header_auth`) against a credential seeded into the
//! metadata store, exercising the same auth path a real client hits.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use s3_api::{AppState, build_router};
use s3_core::{NodeId, OwnerId, VolumeId};
use s3_metadata::{Credential, MetadataStore, RedbMetadataStore};
use s3_object::{BucketService, ObjectService};
use s3_storage::LocalVolumeManager;

const ACCESS_KEY: &str = "AKIATESTACCESSKEY";
const SECRET_KEY: &str = "test-secret-key-do-not-use-in-prod";
const REGION: &str = "us-east-1";

struct TestApp {
    router: Router,
    metadata: Arc<RedbMetadataStore>,
}

async fn test_app() -> TestApp {
    let meta_dir = tempfile::tempdir().unwrap();
    let metadata = Arc::new(
        RedbMetadataStore::open(meta_dir.keep().join("meta.redb"))
            .await
            .unwrap(),
    );

    let owner_id = OwnerId::new();
    metadata
        .put_credential(Credential {
            access_key: ACCESS_KEY.to_string(),
            secret_key: SECRET_KEY.to_string(),
            owner_id,
            enabled: true,
            created_at: time::OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();

    let node_id = NodeId::new();
    let vol_dir = tempfile::tempdir().unwrap();
    let volumes = Arc::new(
        LocalVolumeManager::open(node_id, vec![vol_dir.keep()])
            .await
            .unwrap(),
    );
    let volume_ids: Vec<VolumeId> = volumes.volume_ids().collect();

    let state = AppState {
        buckets: Arc::new(BucketService::new(metadata.clone())),
        objects: Arc::new(ObjectService::new(
            metadata.clone(),
            volumes,
            node_id,
            volume_ids,
        )),
        credentials: metadata.clone(),
        region: REGION.to_string(),
    };
    TestApp {
        router: build_router(state),
        metadata,
    }
}

fn signed_request_with(
    access_key: &str,
    secret_key: &str,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> Request<Body> {
    let payload_hash = sha256_hex(&body);
    let amz_date = s3_auth::amz_date_now();
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));

    let headers = vec![
        ("host".to_string(), "localhost".to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
    ];
    let signed_headers = vec![
        "host".to_string(),
        "x-amz-content-sha256".to_string(),
        "x-amz-date".to_string(),
    ];

    let authorization = s3_auth::sign_header_auth(
        method,
        path,
        query,
        &headers,
        &signed_headers,
        &payload_hash,
        access_key,
        secret_key,
        REGION,
        &amz_date,
    );

    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in &headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .header(header::AUTHORIZATION, authorization)
        .body(Body::from(body))
        .unwrap()
}

fn sha256_hex(data: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(data).into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds a fully SigV4-signed request the same way a real client would: compute the
/// payload hash, mint `x-amz-date`, sign, attach `Authorization`.
fn signed_request(method: &str, uri: &str, body: Vec<u8>) -> Request<Body> {
    let payload_hash = sha256_hex(&body);
    let amz_date = s3_auth::amz_date_now();
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));

    let headers = vec![
        ("host".to_string(), "localhost".to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
    ];
    let signed_headers = vec![
        "host".to_string(),
        "x-amz-content-sha256".to_string(),
        "x-amz-date".to_string(),
    ];

    let authorization = s3_auth::sign_header_auth(
        method,
        path,
        query,
        &headers,
        &signed_headers,
        &payload_hash,
        ACCESS_KEY,
        SECRET_KEY,
        REGION,
        &amz_date,
    );

    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in &headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .header(header::AUTHORIZATION, authorization)
        .body(Body::from(body))
        .unwrap()
}

fn req(method: &str, uri: &str) -> Request<Body> {
    signed_request(method, uri, Vec::new())
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn unsigned_requests_are_rejected() {
    let app = test_app().await;
    let unsigned = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(unsigned).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = body_string(res).await;
    assert!(
        body.contains("<Code>AccessDenied</Code>"),
        "body was: {body}"
    );
}

#[tokio::test]
async fn a_wrong_secret_is_rejected() {
    let app = test_app().await;
    let payload_hash = sha256_hex(b"");
    let amz_date = s3_auth::amz_date_now();
    let headers = vec![
        ("host".to_string(), "localhost".to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
    ];
    let signed_headers = vec![
        "host".to_string(),
        "x-amz-content-sha256".to_string(),
        "x-amz-date".to_string(),
    ];
    let bad_auth = s3_auth::sign_header_auth(
        "GET",
        "/",
        "",
        &headers,
        &signed_headers,
        &payload_hash,
        ACCESS_KEY,
        "totally-wrong-secret",
        REGION,
        &amz_date,
    );
    let mut builder = Request::builder().method("GET").uri("/");
    for (k, v) in &headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder
        .header(header::AUTHORIZATION, bad_auth)
        .body(Body::empty())
        .unwrap();

    let res = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = body_string(res).await;
    assert!(
        body.contains("<Code>SignatureDoesNotMatch</Code>"),
        "body was: {body}"
    );
}

#[tokio::test]
async fn create_head_list_delete_bucket() {
    let app = test_app().await;

    let res = app
        .router
        .clone()
        .oneshot(req("PUT", "/photos"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/photos");

    let res = app
        .router
        .clone()
        .oneshot(req("HEAD", "/photos"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .router
        .clone()
        .oneshot(req("HEAD", "/does-not-exist"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = app.router.clone().oneshot(req("GET", "/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("<Name>photos</Name>"), "body was: {body}");

    let res = app
        .router
        .clone()
        .oneshot(req("DELETE", "/photos"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = app
        .router
        .clone()
        .oneshot(req("HEAD", "/photos"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_bucket_creation_is_conflict() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/dup"))
        .await
        .unwrap();

    let res = app
        .router
        .clone()
        .oneshot(req("PUT", "/dup"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = body_string(res).await;
    assert!(body.contains("<Code>BucketAlreadyExists</Code>"));
    assert!(body.contains("<RequestId>"));
}

#[tokio::test]
async fn a_different_credential_cannot_touch_someone_elses_bucket() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/private"))
        .await
        .unwrap();

    // A second, fully valid, *different* credential belonging to a different owner.
    let other_access_key = "AKIAOTHERACCESSKEY";
    let other_secret_key = "another-test-secret-key";
    app.metadata
        .put_credential(Credential {
            access_key: other_access_key.to_string(),
            secret_key: other_secret_key.to_string(),
            owner_id: OwnerId::new(),
            enabled: true,
            created_at: time::OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();

    // Authentic signature, wrong owner: denied, not 404 -- the bucket does exist, this
    // credential just isn't allowed to see it. (HEAD responses never carry a body per
    // HTTP semantics, so only the status is checked here; the DELETE case below
    // checks the same denial with a real XML body.)
    let res = app
        .router
        .clone()
        .oneshot(signed_request_with(
            other_access_key,
            other_secret_key,
            "HEAD",
            "/private",
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .router
        .clone()
        .oneshot(signed_request_with(
            other_access_key,
            other_secret_key,
            "DELETE",
            "/private",
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body = body_string(res).await;
    assert!(
        body.contains("<Code>AccessDenied</Code>"),
        "body was: {body}"
    );

    // The original owner is unaffected.
    let res = app
        .router
        .clone()
        .oneshot(req("HEAD", "/private"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn invalid_bucket_name_is_bad_request() {
    let app = test_app().await;
    let res = app.router.clone().oneshot(req("PUT", "/AB")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = body_string(res).await;
    assert!(body.contains("<Code>InvalidBucketName</Code>"));
}

#[tokio::test]
async fn put_get_head_delete_object_and_sha256_roundtrip() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/data"))
        .await
        .unwrap();

    let payload = b"the quick brown fox jumps over the lazy dog".repeat(100);
    let expected_sha256 = sha256_hex(&payload);

    let mut put_req = signed_request("PUT", "/data/animals/fox.txt", payload.clone());
    put_req
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
    put_req
        .headers_mut()
        .insert("x-amz-meta-owner", "test-suite".parse().unwrap());
    let res = app.router.clone().oneshot(put_req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let put_etag = res
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let res = app
        .router
        .clone()
        .oneshot(req("HEAD", "/data/animals/fox.txt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::CONTENT_LENGTH).unwrap(),
        &payload.len().to_string()
    );
    assert_eq!(
        res.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        put_etag
    );

    let res = app
        .router
        .clone()
        .oneshot(req("GET", "/data/animals/fox.txt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        put_etag
    );
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), payload.as_slice());
    assert_eq!(sha256_hex(&body), expected_sha256);

    let res = app
        .router
        .clone()
        .oneshot(req("DELETE", "/data/animals/fox.txt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = app
        .router
        .clone()
        .oneshot(req("GET", "/data/animals/fox.txt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_string(res).await;
    assert!(body.contains("<Code>NoSuchKey</Code>"));
}

#[tokio::test]
async fn get_object_from_missing_bucket_is_not_found() {
    let app = test_app().await;
    let res = app
        .router
        .clone()
        .oneshot(req("GET", "/nope/key"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_string(res).await;
    assert!(body.contains("<Code>NoSuchBucket</Code>"));
}

#[tokio::test]
async fn delete_non_empty_bucket_is_conflict_then_succeeds_once_empty() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/bkt"))
        .await
        .unwrap();
    app.router
        .clone()
        .oneshot(signed_request("PUT", "/bkt/k", b"x".to_vec()))
        .await
        .unwrap();

    let res = app
        .router
        .clone()
        .oneshot(req("DELETE", "/bkt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body = body_string(res).await;
    assert!(body.contains("<Code>BucketNotEmpty</Code>"));

    app.router
        .clone()
        .oneshot(req("DELETE", "/bkt/k"))
        .await
        .unwrap();
    let res = app
        .router
        .clone()
        .oneshot(req("DELETE", "/bkt"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn list_objects_v2_prefix_delimiter_and_pagination() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/list"))
        .await
        .unwrap();
    for key in ["a/1.txt", "a/2.txt", "b/1.txt"] {
        app.router
            .clone()
            .oneshot(signed_request(
                "PUT",
                &format!("/list/{key}"),
                b"x".to_vec(),
            ))
            .await
            .unwrap();
    }

    // Delimiter grouping happens on what comes *after* the prefix: with no prefix and
    // delimiter "/", "a/1.txt" and "a/2.txt" collapse into common prefix "a/", and
    // "b/1.txt" collapses into "b/" -- none of them appear as individual `<Contents>`.
    let res = app
        .router
        .clone()
        .oneshot(req("GET", "/list?list-type=2&delimiter=/"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res).await;
    assert!(body.contains("<KeyCount>2</KeyCount>"), "body was: {body}");
    assert!(body.contains("<CommonPrefixes><Prefix>a/</Prefix></CommonPrefixes>"));
    assert!(body.contains("<CommonPrefixes><Prefix>b/</Prefix></CommonPrefixes>"));
    assert!(!body.contains("<Contents>"));

    let res = app
        .router
        .clone()
        .oneshot(req("GET", "/list?list-type=2&max-keys=1"))
        .await
        .unwrap();
    let body = body_string(res).await;
    assert!(body.contains("<IsTruncated>true</IsTruncated>"));
    assert!(body.contains("<NextContinuationToken>"));
}

#[tokio::test]
async fn response_headers_carry_a_request_id() {
    let app = test_app().await;
    let res = app.router.clone().oneshot(req("GET", "/")).await.unwrap();
    assert!(res.headers().get("x-amz-request-id").is_some());
}

#[tokio::test]
async fn a_valid_presigned_url_grants_access_without_an_authorization_header() {
    let app = test_app().await;
    app.router
        .clone()
        .oneshot(req("PUT", "/presign-bucket"))
        .await
        .unwrap();
    app.router
        .clone()
        .oneshot(signed_request(
            "PUT",
            "/presign-bucket/k",
            b"presigned-data".to_vec(),
        ))
        .await
        .unwrap();

    let amz_date = s3_auth::amz_date_now();
    let headers = vec![("host".to_string(), "localhost".to_string())];
    let query = s3_auth::sign_presigned_query(
        "GET",
        "/presign-bucket/k",
        &headers,
        &["host".to_string()],
        ACCESS_KEY,
        SECRET_KEY,
        REGION,
        &amz_date,
        3600,
    );

    let request = Request::builder()
        .method("GET")
        .uri(format!("/presign-bucket/k?{query}"))
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let res = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"presigned-data");
}
