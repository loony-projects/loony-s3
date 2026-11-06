//! Signing in end to end against a fake identity provider listening on a real socket, so
//! the real HTTP exchange runs. The tokens it issues are real EdDSA JWTs, and each test
//! gets it to issue exactly the bad token it needs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use loony_console::{ConsoleConfig, ConsoleState, build_router, delete_expired_credentials};
use loony_core::OwnerId;
use loony_metadata::{Credential, CredentialKind, MetadataStore, RedbMetadataStore, UserAccess};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const REGION: &str = "us-east-1";
const CLIENT_ID: &str = "test-client";
const CLIENT_SECRET: &str = "test-client-secret";
const REDIRECT_URI: &str = "http://localhost:5173/callback";
const NONCE: &str = "test-nonce";
const VERIFIER: &str = "test-verifier";

struct SigningKey {
    kid: String,
    encoding: EncodingKey,
    public_x: String,
}

fn new_key(kid: &str) -> SigningKey {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    SigningKey {
        kid: kid.to_string(),
        encoding: EncodingKey::from_ed_der(pkcs8.as_ref()),
        public_x: URL_SAFE_NO_PAD.encode(pair.public_key().as_ref()),
    }
}

#[derive(Default)]
struct IdpState {
    published: Vec<(String, String)>,
    /// Which token each outstanding code turns into. Taken on use, so a code works once.
    codes: HashMap<String, String>,
    token_requests: u32,
    jwks_requests: u32,
    seen_forms: Vec<HashMap<String, String>>,
    fail_token_endpoint: bool,
}

#[derive(Clone)]
struct FakeIdp {
    issuer: String,
    state: Arc<Mutex<IdpState>>,
}

async fn token_endpoint(
    State(idp): State<FakeIdp>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let mut state = idp.state.lock().unwrap();
    state.token_requests += 1;
    state.seen_forms.push(form.clone());
    if state.fail_token_endpoint {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
    }

    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        );
    }
    let token = form.get("code").and_then(|code| state.codes.remove(code));
    match token {
        Some(token) => (StatusCode::OK, Json(json!({"id_token": token}))),
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        ),
    }
}

async fn jwks_endpoint(State(idp): State<FakeIdp>) -> Json<Value> {
    let mut state = idp.state.lock().unwrap();
    state.jwks_requests += 1;
    let keys: Vec<Value> = state
        .published
        .iter()
        .map(|(kid, x)| {
            json!({"kid": kid, "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "x": x})
        })
        .collect();
    Json(json!({ "keys": keys }))
}

impl FakeIdp {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idp = FakeIdp {
            issuer: format!("http://{}", listener.local_addr().unwrap()),
            state: Arc::default(),
        };
        let router = Router::new()
            .route("/oauth/token", post(token_endpoint))
            .route("/.well-known/jwks.json", get(jwks_endpoint))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        idp
    }

    fn publish(&self, key: &SigningKey) {
        self.state
            .lock()
            .unwrap()
            .published
            .push((key.kid.clone(), key.public_x.clone()));
    }

    fn good_claims(&self) -> Value {
        json!({
            "iss": self.issuer,
            "sub": "user-1",
            "aud": CLIENT_ID,
            "exp": OffsetDateTime::now_utc().unix_timestamp() + 300,
            "iat": OffsetDateTime::now_utc().unix_timestamp(),
            "nonce": NONCE,
            "email": "person@example.com",
            "email_verified": true,
            "name": "A Person",
        })
    }

    fn code_for_token(&self, code: &str, token: String) -> String {
        self.state
            .lock()
            .unwrap()
            .codes
            .insert(code.to_string(), token);
        code.to_string()
    }

    fn code_for(&self, code: &str, key: &SigningKey, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(key.kid.clone());
        let token = jsonwebtoken::encode(&header, claims, &key.encoding).unwrap();
        self.code_for_token(code, token)
    }

    fn token_requests(&self) -> u32 {
        self.state.lock().unwrap().token_requests
    }

    fn jwks_requests(&self) -> u32 {
        self.state.lock().unwrap().jwks_requests
    }
}

struct Harness {
    idp: FakeIdp,
    key: SigningKey,
    router: Router,
    metadata: Arc<RedbMetadataStore>,
    _dir: tempfile::TempDir,
}

fn config(issuer: &str) -> ConsoleConfig {
    ConsoleConfig {
        issuer: issuer.to_string(),
        internal_url: None,
        client_id: CLIENT_ID.to_string(),
        client_secret: CLIENT_SECRET.to_string(),
        redirect_uris: vec![REDIRECT_URI.to_string()],
        session_ttl: Duration::from_secs(8 * 60 * 60),
        client_ip_header: None,
    }
}

fn audit(dir: &tempfile::TempDir) -> Arc<loony_admin::AuditLog> {
    Arc::new(loony_admin::AuditLog::new(dir.path().join("audit.jsonl")))
}

async fn open_metadata(dir: &tempfile::TempDir) -> Arc<RedbMetadataStore> {
    Arc::new(
        RedbMetadataStore::open(dir.path().join("meta.redb"))
            .await
            .unwrap(),
    )
}

async fn harness() -> Harness {
    let idp = FakeIdp::start().await;
    let key = new_key("key-1");
    idp.publish(&key);
    let dir = tempfile::tempdir().unwrap();
    let metadata = open_metadata(&dir).await;
    let state = ConsoleState::new(
        config(&idp.issuer),
        metadata.clone(),
        REGION.into(),
        audit(&dir),
    )
    .unwrap();
    Harness {
        idp,
        key,
        router: build_router(state),
        metadata,
        _dir: dir,
    }
}

fn session_request(code: &str, redirect_uri: &str, nonce: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/_console/v1/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "code": code,
                "code_verifier": VERIFIER,
                "redirect_uri": redirect_uri,
                "nonce": nonce,
            })
            .to_string(),
        ))
        .unwrap()
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, body)
}

impl Harness {
    async fn sign_in_with(&self, code: String) -> (StatusCode, HeaderMap, Value) {
        send(&self.router, session_request(&code, REDIRECT_URI, NONCE)).await
    }

    async fn sign_in(&self) -> Value {
        let code = self
            .idp
            .code_for("good-code", &self.key, &self.idp.good_claims());
        let (status, _, body) = self.sign_in_with(code).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn assert_refused(&self, code: String) {
        let users_before = self.metadata.list_users().await.unwrap().len();
        let (status, _, body) = self.sign_in_with(code).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["code"], "sign_in_failed");
        assert!(body.get("access_key").is_none());
        assert_eq!(
            self.metadata.list_users().await.unwrap().len(),
            users_before
        );
        assert!(self.metadata.list_credentials().await.unwrap().is_empty());
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn signed(method: &str, uri: &str, access_key: &str, secret_key: &str) -> Request<Body> {
    let amz_date = loony_auth::amz_date_now();
    let payload_hash = sha256_hex(b"");
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
    let authorization = loony_auth::sign_header_auth(
        method,
        uri,
        "",
        &headers,
        &signed_headers,
        &payload_hash,
        access_key,
        secret_key,
        REGION,
        &amz_date,
    );
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in &headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .header(header::AUTHORIZATION, authorization)
        .body(Body::empty())
        .unwrap()
}

fn signed_with(session: &Value, method: &str, uri: &str) -> Request<Body> {
    signed(
        method,
        uri,
        session["access_key"].as_str().unwrap(),
        session["secret_key"].as_str().unwrap(),
    )
}

#[tokio::test]
async fn a_good_sign_in_returns_a_working_session_key() {
    let h = harness().await;
    let session = h.sign_in().await;

    assert_eq!(session["user"]["email"], "person@example.com");
    assert_eq!(session["user"]["email_verified"], true);
    assert_eq!(session["user"]["name"], "A Person");
    assert_eq!(session["user"]["can_create_projects"], false);

    let stored = h
        .metadata
        .get_credential(session["access_key"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.kind, CredentialKind::UserSession);
    assert!(!stored.admin);
    assert!(stored.user_id.is_some());
    let remaining = stored.expires_at.unwrap() - OffsetDateTime::now_utc();
    assert!(remaining > time::Duration::hours(7) && remaining <= time::Duration::hours(8));

    let (status, _, me) = send(&h.router, signed_with(&session, "GET", "/_console/v1/me")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me, session["user"]);
}

#[tokio::test]
async fn the_exchange_sends_what_the_identity_provider_needs() {
    let h = harness().await;
    h.sign_in().await;

    let state = h.idp.state.lock().unwrap();
    let form = &state.seen_forms[0];
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "good-code");
    assert_eq!(form["redirect_uri"], REDIRECT_URI);
    assert_eq!(form["code_verifier"], VERIFIER);
}

#[tokio::test]
async fn signing_in_again_finds_the_same_user_and_updates_them() {
    let h = harness().await;
    let first = h.sign_in().await;

    let mut claims = h.idp.good_claims();
    claims["email"] = json!("renamed@example.com");
    let code = h.idp.code_for("second", &h.key, &claims);
    let (status, _, second) = h.sign_in_with(code).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(second["user"]["id"], first["user"]["id"]);
    assert_eq!(second["user"]["email"], "renamed@example.com");
    assert_eq!(h.metadata.list_users().await.unwrap().len(), 1);
    assert_ne!(second["access_key"], first["access_key"]);
}

#[tokio::test]
async fn sign_in_responses_are_not_cached() {
    let h = harness().await;
    let code = h.idp.code_for("code", &h.key, &h.idp.good_claims());
    let (_, headers, _) = h.sign_in_with(code).await;
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn a_token_signed_by_the_wrong_key_is_refused() {
    let h = harness().await;
    // Same key id as the real key, but not the real key.
    let forger = new_key("key-1");
    let code = h.idp.code_for("forged", &forger, &h.idp.good_claims());
    h.assert_refused(code).await;
}

#[tokio::test]
async fn a_tampered_token_is_refused() {
    let h = harness().await;
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(h.key.kid.clone());
    let token = jsonwebtoken::encode(&header, &h.idp.good_claims(), &h.key.encoding).unwrap();

    let mut claims = h.idp.good_claims();
    claims["sub"] = json!("someone-else");
    let forged_payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    let parts: Vec<&str> = token.split('.').collect();
    let tampered = format!("{}.{forged_payload}.{}", parts[0], parts[2]);

    let code = h.idp.code_for_token("tampered", tampered);
    h.assert_refused(code).await;
}

#[tokio::test]
async fn tokens_with_any_other_algorithm_are_refused() {
    let h = harness().await;

    // The classic confusion attack: sign with HMAC, using the public key as the secret.
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(h.key.kid.clone());
    let token = jsonwebtoken::encode(
        &header,
        &h.idp.good_claims(),
        &EncodingKey::from_secret(h.key.public_x.as_bytes()),
    )
    .unwrap();
    let code = h.idp.code_for_token("hmac", token);
    h.assert_refused(code).await;

    // And no signature at all.
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "none", "kid": h.key.kid}).to_string());
    let payload = URL_SAFE_NO_PAD.encode(h.idp.good_claims().to_string());
    let code = h
        .idp
        .code_for_token("unsigned", format!("{header}.{payload}."));
    h.assert_refused(code).await;
}

#[tokio::test]
async fn tokens_with_wrong_claims_are_refused() {
    let h = harness().await;
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for (name, field, value) in [
        ("wrong-issuer", "iss", json!("https://evil.example")),
        ("wrong-audience", "aud", json!("some-other-app")),
        ("expired", "exp", json!(now - 3600)),
        ("wrong-nonce", "nonce", json!("not-the-nonce")),
    ] {
        let mut claims = h.idp.good_claims();
        claims[field] = value;
        let code = h.idp.code_for(name, &h.key, &claims);
        h.assert_refused(code).await;
    }

    for field in ["nonce", "sub", "exp", "iss", "aud"] {
        let mut claims = h.idp.good_claims();
        claims.as_object_mut().unwrap().remove(field);
        let code = h.idp.code_for(&format!("missing-{field}"), &h.key, &claims);
        h.assert_refused(code).await;
    }
}

#[tokio::test]
async fn a_code_only_works_once() {
    let h = harness().await;
    let code = h.idp.code_for("once", &h.key, &h.idp.good_claims());
    let (status, _, _) = h.sign_in_with(code.clone()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = h.sign_in_with(code).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "sign_in_failed");
}

#[tokio::test]
async fn an_unlisted_return_address_never_reaches_the_identity_provider() {
    let h = harness().await;
    let code = h.idp.code_for("code", &h.key, &h.idp.good_claims());
    for address in [
        "https://evil.example/callback",
        "http://localhost:5173/callback/extra",
        "http://localhost:5173",
    ] {
        let (status, _, body) = send(&h.router, session_request(&code, address, NONCE)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{address}");
        assert_eq!(body["code"], "invalid_request");
    }
    assert_eq!(h.idp.token_requests(), 0);
}

#[tokio::test]
async fn malformed_requests_are_refused() {
    let h = harness().await;

    let not_json = Request::builder()
        .method("POST")
        .uri("/_console/v1/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("not json"))
        .unwrap();
    let (status, _, body) = send(&h.router, not_json).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, _, _) = send(&h.router, session_request("", REDIRECT_URI, NONCE)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = send(&h.router, session_request("code", REDIRECT_URI, "")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert_eq!(h.idp.token_requests(), 0);
}

#[tokio::test]
async fn an_unavailable_identity_provider_is_reported_as_such() {
    let h = harness().await;
    h.idp.state.lock().unwrap().fail_token_endpoint = true;
    let (status, _, body) = h.sign_in_with("code".to_string()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "identity_provider_unavailable");
}

#[tokio::test]
async fn an_unreachable_identity_provider_is_reported_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = open_metadata(&dir).await;
    // Nothing listens on this port.
    let state = ConsoleState::new(
        config("http://127.0.0.1:1"),
        metadata,
        REGION.into(),
        audit(&dir),
    )
    .unwrap();
    let router = build_router(state);
    let (status, _, body) = send(&router, session_request("code", REDIRECT_URI, NONCE)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "identity_provider_unavailable");
}

#[tokio::test]
async fn known_keys_come_from_the_cache() {
    let h = harness().await;
    h.sign_in().await;
    assert_eq!(h.idp.jwks_requests(), 1);

    let code = h.idp.code_for("cached", &h.key, &h.idp.good_claims());
    assert_eq!(h.sign_in_with(code).await.0, StatusCode::OK);
    assert_eq!(h.idp.jwks_requests(), 1);
}

#[tokio::test]
async fn a_first_token_with_a_rotated_key_fetches_the_new_key() {
    let h = harness().await;
    let rotated = new_key("key-2");
    h.idp.publish(&rotated);
    let code = h.idp.code_for("rotated", &rotated, &h.idp.good_claims());
    assert_eq!(h.sign_in_with(code).await.0, StatusCode::OK);
}

#[tokio::test]
async fn unknown_key_ids_cannot_hammer_the_identity_provider() {
    let h = harness().await;
    h.sign_in().await;
    assert_eq!(h.idp.jwks_requests(), 1);

    for attempt in 0..5 {
        let unknown = new_key(&format!("unknown-{attempt}"));
        let code = h
            .idp
            .code_for(&format!("c{attempt}"), &unknown, &h.idp.good_claims());
        assert_eq!(h.sign_in_with(code).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(h.idp.jwks_requests(), 1);
}

#[tokio::test]
async fn a_disabled_user_is_refused_and_their_session_stops_at_once() {
    let h = harness().await;
    let session = h.sign_in().await;
    let user = h.metadata.list_users().await.unwrap().remove(0);

    h.metadata
        .set_user_access(
            user.user_id,
            UserAccess {
                enabled: false,
                can_create_projects: false,
            },
        )
        .await
        .unwrap();

    let (status, _, body) = send(&h.router, signed_with(&session, "GET", "/_console/v1/me")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "account_disabled");

    let credentials_before = h.metadata.list_credentials().await.unwrap().len();
    let code = h.idp.code_for("again", &h.key, &h.idp.good_claims());
    let (status, _, body) = h.sign_in_with(code).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "account_disabled");
    assert_eq!(
        h.metadata.list_credentials().await.unwrap().len(),
        credentials_before,
        "no new key for a disabled user"
    );
}

#[tokio::test]
async fn signing_out_ends_the_session() {
    let h = harness().await;
    let session = h.sign_in().await;

    let (status, _, _) = send(
        &h.router,
        signed_with(&session, "DELETE", "/_console/v1/session"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, body) = send(&h.router, signed_with(&session, "GET", "/_console/v1/me")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
    assert!(h.metadata.list_credentials().await.unwrap().is_empty());
}

#[tokio::test]
async fn signing_out_ends_only_that_session() {
    let h = harness().await;
    let first = h.sign_in().await;
    let code = h.idp.code_for("second", &h.key, &h.idp.good_claims());
    let (_, _, second) = h.sign_in_with(code).await;

    send(
        &h.router,
        signed_with(&first, "DELETE", "/_console/v1/session"),
    )
    .await;

    let (status, _, _) = send(&h.router, signed_with(&second, "GET", "/_console/v1/me")).await;
    assert_eq!(status, StatusCode::OK);
}

async fn put_key(
    metadata: &RedbMetadataStore,
    access_key: &str,
    kind: CredentialKind,
    expires_at: Option<OffsetDateTime>,
    user_id: Option<loony_core::UserId>,
) {
    metadata
        .put_credential(Credential {
            access_key: access_key.to_string(),
            secret_key: "secret".to_string(),
            owner_id: OwnerId::new(),
            enabled: true,
            admin: false,
            kind,
            user_id,
            expires_at,
            project_id: None,
            name: None,
            session_of: None,
            last_used_at: None,
            created_at: OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_expired_session_says_so() {
    let h = harness().await;
    h.sign_in().await;
    let user = h.metadata.list_users().await.unwrap().remove(0);
    let past = OffsetDateTime::now_utc() - time::Duration::seconds(1);
    put_key(
        &h.metadata,
        "EXPIRED",
        CredentialKind::UserSession,
        Some(past),
        Some(user.user_id),
    )
    .await;

    let (status, _, body) = send(
        &h.router,
        signed("GET", "/_console/v1/me", "EXPIRED", "secret"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "session_expired");
}

#[tokio::test]
async fn a_standard_key_cannot_use_the_console() {
    let h = harness().await;
    put_key(
        &h.metadata,
        "STANDARD",
        CredentialKind::Standard,
        None,
        None,
    )
    .await;

    let (status, _, body) = send(
        &h.router,
        signed("GET", "/_console/v1/me", "STANDARD", "secret"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "wrong_key");
}

#[tokio::test]
async fn console_requests_need_a_valid_signature() {
    let h = harness().await;
    let session = h.sign_in().await;

    let unsigned = Request::builder()
        .uri("/_console/v1/me")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.router, unsigned).await.0, StatusCode::UNAUTHORIZED);

    let wrong_secret = signed(
        "GET",
        "/_console/v1/me",
        session["access_key"].as_str().unwrap(),
        "not-the-secret",
    );
    assert_eq!(
        send(&h.router, wrong_secret).await.0,
        StatusCode::UNAUTHORIZED
    );

    let unsigned_delete = Request::builder()
        .method("DELETE")
        .uri("/_console/v1/session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&h.router, unsigned_delete).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_body_that_does_not_match_its_signature_is_refused() {
    let h = harness().await;
    let session = h.sign_in().await;

    let (parts, _) = signed_with(&session, "GET", "/_console/v1/me").into_parts();
    let tampered = Request::from_parts(parts, Body::from("extra"));
    let (status, _, body) = send(&h.router, tampered).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

#[tokio::test]
async fn too_many_attempts_are_slowed_down() {
    let idp = FakeIdp::start().await;
    let dir = tempfile::tempdir().unwrap();
    let metadata = open_metadata(&dir).await;
    let mut settings = config(&idp.issuer);
    settings.client_ip_header = Some("cf-connecting-ip".to_string());
    let state = ConsoleState::new(settings, metadata, REGION.into(), audit(&dir))
        .unwrap()
        .with_sign_in_limit(2, Duration::from_secs(60));
    let router = build_router(state);

    let from = |ip: &str| {
        let mut request = session_request("code", REDIRECT_URI, NONCE);
        request
            .headers_mut()
            .insert("cf-connecting-ip", ip.parse().unwrap());
        request
    };
    for _ in 0..2 {
        assert_ne!(
            send(&router, from("192.0.2.1")).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let (status, headers, body) = send(&router, from("192.0.2.1")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "too_many_requests");
    assert_eq!(headers.get(header::RETRY_AFTER).unwrap(), "60");

    // Someone else isn't affected.
    assert_ne!(
        send(&router, from("192.0.2.2")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        idp.token_requests(),
        3,
        "refused attempts never reach the provider"
    );
}

#[tokio::test]
async fn only_the_consoles_own_pages_may_call_the_api() {
    let h = harness().await;
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/_console/v1/session")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "content-type,authorization",
            )
            .body(Body::empty())
            .unwrap()
    };

    let (_, headers, _) = send(&h.router, preflight("http://localhost:5173")).await;
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "http://localhost:5173"
    );
    assert!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("authorization")
    );

    let (_, headers, _) = send(&h.router, preflight("https://evil.example")).await;
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn cleanup_removes_only_expired_keys() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = open_metadata(&dir).await;
    let now = OffsetDateTime::now_utc();
    let hour = time::Duration::hours(1);
    put_key(
        &metadata,
        "PAST",
        CredentialKind::UserSession,
        Some(now - hour),
        None,
    )
    .await;
    put_key(
        &metadata,
        "FUTURE",
        CredentialKind::UserSession,
        Some(now + hour),
        None,
    )
    .await;
    put_key(&metadata, "FOREVER", CredentialKind::Standard, None, None).await;

    assert_eq!(
        delete_expired_credentials(metadata.as_ref(), now)
            .await
            .unwrap(),
        1
    );
    let remaining: HashSet<String> = metadata
        .list_credentials()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.access_key)
        .collect();
    assert_eq!(
        remaining,
        HashSet::from(["FUTURE".to_string(), "FOREVER".to_string()])
    );
}

/// Both APIs together, as `loony-server` serves them.
async fn merged_with_the_ls3_api(h: &Harness, metadata: Arc<RedbMetadataStore>) -> Router {
    use loony_core::{NodeId, VolumeId};
    let node_id = NodeId::new();
    let volumes = Arc::new(
        loony_storage::LocalVolumeManager::open(node_id, vec![h._dir.path().join("volumes")])
            .await
            .unwrap(),
    );
    let volume_ids: Vec<VolumeId> = volumes.volume_ids().collect();
    let api = loony_api::build_router(loony_api::AppState {
        buckets: Arc::new(loony_object::BucketService::new(metadata.clone())),
        objects: Arc::new(loony_object::ObjectService::new(
            metadata.clone(),
            volumes,
            node_id,
            volume_ids,
        )),
        credentials: metadata,
        region: REGION.to_string(),
    });
    api.merge(h.router.clone())
}

#[tokio::test]
async fn both_apis_work_side_by_side_and_keep_their_own_keys() {
    let h = harness().await;
    let router = merged_with_the_ls3_api(&h, h.metadata.clone()).await;

    // A session key works on the console, and is refused by the LS3 API.
    let code = h.idp.code_for("code", &h.key, &h.idp.good_claims());
    let (status, _, session) = send(&router, session_request(&code, REDIRECT_URI, NONCE)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&router, signed_with(&session, "GET", "/_console/v1/me")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&router, signed_with(&session, "PUT", "/session-bucket")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(h.metadata.list_all_buckets().await.unwrap().is_empty());

    // A standard key is the other way around.
    put_key(
        &h.metadata,
        "STANDARD",
        CredentialKind::Standard,
        None,
        None,
    )
    .await;
    let (status, _, _) = send(
        &router,
        signed("PUT", "/normal-bucket", "STANDARD", "secret"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        &router,
        signed("GET", "/_console/v1/me", "STANDARD", "secret"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Anything else under the console's prefix falls to the LS3 API, which wants a signature.
    let unknown = Request::builder()
        .uri("/_console/v1/nothing-here")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, unknown).await.0, StatusCode::FORBIDDEN);

    // Each keeps its own CORS policy: the LS3 API stays open to any origin.
    let preflight = |path: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri(path)
            .header(header::ORIGIN, "https://anywhere.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .unwrap()
    };
    let (_, headers, _) = send(&router, preflight("/normal-bucket/file")).await;
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "*"
    );
    let (_, headers, _) = send(&router, preflight("/_console/v1/me")).await;
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}
