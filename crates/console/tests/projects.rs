//! Projects through the real routers, with the LS3 API alongside: "this key stops working"
//! only means something when it's shown on the API the key is for. Sessions are seeded
//! directly, since `session.rs` covers how they're issued.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use loony_console::{
    ConsoleConfig, ConsoleState, MAX_ACCESS_KEYS_PER_PROJECT, MAX_PROJECTS_PER_USER, build_router,
    delete_expired_credentials,
};
use loony_core::{NodeId, OwnerId, UserId, VolumeId};
use loony_metadata::{
    CreateInvite, Credential, CredentialKind, MetadataStore, ProjectRole, RedbMetadataStore,
    UserAccess, UserSignIn,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tower::ServiceExt;

const REGION: &str = "us-east-1";

struct Harness {
    router: Router,
    metadata: Arc<RedbMetadataStore>,
    audit_path: PathBuf,
    /// Deleted with the harness. Some tests build dozens, so they mustn't pile up.
    _dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Person {
    id: UserId,
    access_key: String,
    secret_key: String,
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn signed(
    method: &str,
    uri: &str,
    access_key: &str,
    secret_key: &str,
    body: Vec<u8>,
) -> Request<Body> {
    let amz_date = loony_auth::amz_date_now();
    let payload_hash = sha256_hex(&body);
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
    let authorization = loony_auth::sign_header_auth(
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
    for (name, value) in &headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

impl Harness {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let metadata = Arc::new(
            RedbMetadataStore::open(dir.join("meta.redb"))
                .await
                .unwrap(),
        );
        let audit_path = dir.join("audit.jsonl");
        // Nothing here signs in through the identity provider, so it's never contacted.
        let state = ConsoleState::new(
            ConsoleConfig {
                issuer: "http://127.0.0.1:1".into(),
                internal_url: None,
                client_id: "client".into(),
                client_secret: "secret".into(),
                redirect_uris: vec!["http://localhost:5173/callback".into()],
                session_ttl: Duration::from_secs(3600),
                client_ip_header: None,
            },
            metadata.clone(),
            REGION.into(),
            Arc::new(loony_admin::AuditLog::new(audit_path.clone())),
        )
        .unwrap();

        let node_id = NodeId::new();
        let volumes = Arc::new(
            loony_storage::LocalVolumeManager::open(node_id, vec![dir.join("volumes")])
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
            credentials: metadata.clone(),
            region: REGION.to_string(),
        });

        Self {
            router: api.merge(build_router(state)),
            metadata,
            audit_path,
            _dir: temp,
        }
    }

    async fn person_with(&self, email: &str, verified: bool, approved: bool) -> Person {
        let user = self
            .metadata
            .record_user_sign_in(UserSignIn {
                issuer: "http://127.0.0.1:1".into(),
                subject: email.into(),
                email: Some(email.into()),
                email_verified: verified,
                name: Some(format!("Name of {email}")),
            })
            .await
            .unwrap();
        self.metadata
            .set_user_access(
                user.user_id,
                UserAccess {
                    enabled: true,
                    can_create_projects: approved,
                },
            )
            .await
            .unwrap();

        let access_key = format!("SESSION{}", user.user_id).replace('-', "");
        let secret_key = format!("secret-{}", user.user_id);
        let now = OffsetDateTime::now_utc();
        self.metadata
            .put_credential(Credential {
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                owner_id: OwnerId::new(),
                enabled: true,
                admin: false,
                kind: CredentialKind::UserSession,
                user_id: Some(user.user_id),
                expires_at: Some(now + time::Duration::hours(1)),
                project_id: None,
                name: None,
                session_of: None,
                last_used_at: None,
                created_at: now,
            })
            .await
            .unwrap();
        Person {
            id: user.user_id,
            access_key,
            secret_key,
        }
    }

    async fn person(&self, email: &str) -> Person {
        self.person_with(email, true, true).await
    }

    async fn raw(&self, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn call(
        &self,
        who: &Person,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        let uri = format!("/_console/v1{path}");
        let (status, bytes) = self
            .raw(signed(
                method,
                &uri,
                &who.access_key,
                &who.secret_key,
                bytes,
            ))
            .await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn ok(&self, who: &Person, method: &str, path: &str, body: Value) -> Value {
        let (status, response) = self.call(who, method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {response}");
        response
    }

    async fn project(&self, who: &Person, name: &str) -> String {
        let project = self
            .ok(who, "POST", "/projects", json!({"name": name}))
            .await;
        project["id"].as_str().unwrap().to_string()
    }

    /// Adds `who` to the project through a real invite.
    async fn join(&self, owner: &Person, project: &str, who: &Person, email: &str, role: &str) {
        let invite = self
            .ok(
                owner,
                "POST",
                &format!("/projects/{project}/invites"),
                json!({"email": email, "role": role}),
            )
            .await;
        let id = invite["id"].as_str().unwrap();
        self.ok(who, "POST", &format!("/invites/{id}/accept"), Value::Null)
            .await;
    }

    async fn new_key(&self, who: &Person, project: &str) -> (String, String) {
        let key = self
            .ok(
                who,
                "POST",
                &format!("/projects/{project}/keys"),
                json!({"name": "An app"}),
            )
            .await;
        (
            key["access_key"].as_str().unwrap().to_string(),
            key["secret_key"].as_str().unwrap().to_string(),
        )
    }

    async fn session_key(&self, who: &Person, project: &str) -> (String, String) {
        let session = self
            .ok(
                who,
                "POST",
                &format!("/projects/{project}/session"),
                Value::Null,
            )
            .await;
        (
            session["access_key"].as_str().unwrap().to_string(),
            session["secret_key"].as_str().unwrap().to_string(),
        )
    }

    async fn s3(&self, key: &(String, String), method: &str, uri: &str, body: &[u8]) -> StatusCode {
        self.raw(signed(method, uri, &key.0, &key.1, body.to_vec()))
            .await
            .0
    }
}

#[tokio::test]
async fn creating_a_project_needs_approval() {
    let h = Harness::new().await;
    let unapproved = h.person_with("new@example.com", true, false).await;

    let (status, body) = h
        .call(&unapproved, "POST", "/projects", json!({"name": "Mine"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "not_approved");

    let approved = h.person("ok@example.com").await;
    let (status, body) = h
        .call(&approved, "POST", "/projects", json!({"name": "  Mine  "}))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "Mine");
    assert_eq!(body["role"], "owner");
    assert_eq!(body["created_by_me"], true);
}

#[tokio::test]
async fn project_names_are_checked() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let long = "x".repeat(101);
    for name in ["", "   ", long.as_str(), "with\nnewline"] {
        let (status, body) = h
            .call(&alice, "POST", "/projects", json!({"name": name}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
        assert_eq!(body["code"], "invalid_request");
    }
    let (status, _) = h
        .call(&alice, "POST", "/projects", json!({"nope": 1}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.metadata.list_projects().await.unwrap().is_empty());
}

#[tokio::test]
async fn people_can_only_create_so_many_projects() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    for i in 0..MAX_PROJECTS_PER_USER {
        h.project(&alice, &format!("Project {i}")).await;
    }
    let (status, body) = h
        .call(&alice, "POST", "/projects", json!({"name": "One too many"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "project_limit_reached");

    // That's per person.
    let bob = h.person("bob@example.com").await;
    h.project(&bob, "Bob's").await;
}

#[tokio::test]
async fn the_project_list_separates_created_joined_and_invited() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;

    let mine = h.project(&bob, "Bob's own").await;
    let theirs = h.project(&alice, "Alice's").await;
    h.join(&alice, &theirs, &bob, "bob@example.com", "member")
        .await;
    let other = h.project(&alice, "Pending").await;
    h.ok(
        &alice,
        "POST",
        &format!("/projects/{other}/invites"),
        json!({"email": "BOB@example.com", "role": "owner"}),
    )
    .await;
    h.project(&alice, "Unrelated").await;

    let list = h.ok(&bob, "GET", "/projects", Value::Null).await;
    assert_eq!(list["created"].as_array().unwrap().len(), 1);
    assert_eq!(list["created"][0]["id"], mine.as_str());
    assert_eq!(list["created"][0]["role"], "owner");
    assert_eq!(list["joined"].as_array().unwrap().len(), 1);
    assert_eq!(list["joined"][0]["id"], theirs.as_str());
    assert_eq!(list["joined"][0]["role"], "member");
    assert_eq!(list["invites"].as_array().unwrap().len(), 1);
    assert_eq!(list["invites"][0]["project_name"], "Pending");
    assert_eq!(list["invites"][0]["role"], "owner");
}

/// Every endpoint against every kind of person. Each row runs on a fresh project.
#[tokio::test]
async fn each_role_can_do_exactly_what_it_should() {
    use StatusCode as S;

    #[derive(Clone, Copy)]
    struct Row {
        method: &'static str,
        path: &'static str,
        body: Option<&'static str>,
        owner: StatusCode,
        member: StatusCode,
    }
    let row = |method, path, body, owner, member| Row {
        method,
        path,
        body,
        owner,
        member,
    };

    let rows = [
        row("GET", "/projects/{p}", None, S::OK, S::OK),
        row(
            "PATCH",
            "/projects/{p}",
            Some(r#"{"name":"New"}"#),
            S::OK,
            S::FORBIDDEN,
        ),
        row("DELETE", "/projects/{p}", None, S::NO_CONTENT, S::FORBIDDEN),
        row("POST", "/projects/{p}/session", None, S::OK, S::OK),
        row("GET", "/projects/{p}/keys", None, S::OK, S::OK),
        row(
            "POST",
            "/projects/{p}/keys",
            Some(r#"{"name":"K"}"#),
            S::CREATED,
            S::CREATED,
        ),
        row(
            "PATCH",
            "/projects/{p}/keys/{k}",
            Some(r#"{"enabled":false}"#),
            S::OK,
            S::OK,
        ),
        row(
            "DELETE",
            "/projects/{p}/keys/{k}",
            None,
            S::NO_CONTENT,
            S::NO_CONTENT,
        ),
        row("GET", "/projects/{p}/members", None, S::OK, S::OK),
        row(
            "PATCH",
            "/projects/{p}/members/{third}",
            Some(r#"{"role":"owner"}"#),
            S::OK,
            S::FORBIDDEN,
        ),
        row(
            "DELETE",
            "/projects/{p}/members/{third}",
            None,
            S::NO_CONTENT,
            S::FORBIDDEN,
        ),
        row("GET", "/projects/{p}/invites", None, S::OK, S::FORBIDDEN),
        row(
            "POST",
            "/projects/{p}/invites",
            Some(r#"{"email":"x@example.com","role":"member"}"#),
            S::CREATED,
            S::FORBIDDEN,
        ),
        row(
            "DELETE",
            "/projects/{p}/invites/{i}",
            None,
            S::NO_CONTENT,
            S::FORBIDDEN,
        ),
    ];

    for row in rows {
        for (label, expected) in [
            ("owner", row.owner),
            ("member", row.member),
            ("outsider", S::NOT_FOUND),
        ] {
            let h = Harness::new().await;
            let owner = h.person("owner@example.com").await;
            let member = h.person("member@example.com").await;
            let third = h.person("third@example.com").await;
            let outsider = h.person("outsider@example.com").await;

            let project = h.project(&owner, "Project").await;
            h.join(&owner, &project, &member, "member@example.com", "member")
                .await;
            h.join(&owner, &project, &third, "third@example.com", "member")
                .await;
            let (key, _) = h.new_key(&owner, &project).await;
            let invite = h
                .ok(
                    &owner,
                    "POST",
                    &format!("/projects/{project}/invites"),
                    json!({"email": "pending@example.com", "role": "member"}),
                )
                .await;

            let path = row
                .path
                .replace("{p}", &project)
                .replace("{k}", &key)
                .replace("{third}", &third.id.to_string())
                .replace("{i}", invite["id"].as_str().unwrap());
            let body = row
                .body
                .map_or(Value::Null, |body| serde_json::from_str(body).unwrap());
            let who = match label {
                "owner" => &owner,
                "member" => &member,
                _ => &outsider,
            };
            let (status, response) = h.call(who, row.method, &path, body).await;
            assert_eq!(
                status, expected,
                "{label}: {} {} -> {response}",
                row.method, row.path
            );
            if label == "outsider" {
                assert_eq!(response["code"], "not_found");
            }
        }
    }
}

#[tokio::test]
async fn an_unknown_project_looks_the_same_as_someone_elses() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "Alice's").await;

    let theirs = h
        .call(&bob, "GET", &format!("/projects/{project}"), Value::Null)
        .await;
    let unknown = h
        .call(
            &bob,
            "GET",
            &format!("/projects/{}", loony_core::ProjectId::new()),
            Value::Null,
        )
        .await;
    assert_eq!(theirs, unknown);
    assert_eq!(theirs.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn one_projects_keys_cannot_touch_another_projects_buckets() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let a = h.project(&alice, "A").await;
    let b = h.project(&bob, "B").await;
    let a_key = h.new_key(&alice, &a).await;
    let b_key = h.new_key(&bob, &b).await;

    assert_eq!(
        h.s3(&a_key, "PUT", "/bucket-of-a", b"").await,
        StatusCode::OK
    );
    assert_eq!(
        h.s3(&a_key, "PUT", "/bucket-of-a/file", b"secret").await,
        StatusCode::OK
    );

    assert_ne!(
        h.s3(&b_key, "GET", "/bucket-of-a/file", b"").await,
        StatusCode::OK
    );
    assert_ne!(
        h.s3(&b_key, "PUT", "/bucket-of-a/planted", b"x").await,
        StatusCode::OK
    );
    assert_ne!(
        h.s3(&b_key, "DELETE", "/bucket-of-a/file", b"").await,
        StatusCode::NO_CONTENT
    );
    assert_ne!(
        h.s3(&b_key, "GET", "/bucket-of-a?list-type=2", b"").await,
        StatusCode::OK
    );

    // And a person in one project can't reach into another through a key's address.
    let (status, _) = h
        .call(
            &bob,
            "PATCH",
            &format!("/projects/{b}/keys/{}", a_key.0),
            json!({"enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .call(
            &bob,
            "DELETE",
            &format!("/projects/{b}/keys/{}", a_key.0),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        h.s3(&a_key, "GET", "/bucket-of-a/file", b"").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn everyone_in_a_project_sees_the_same_buckets() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "Shared").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;

    let alice_session = h.session_key(&alice, &project).await;
    let bob_session = h.session_key(&bob, &project).await;
    let app_key = h.new_key(&bob, &project).await;

    assert_eq!(
        h.s3(&alice_session, "PUT", "/shared-bucket", b"").await,
        StatusCode::OK
    );
    assert_eq!(
        h.s3(&bob_session, "PUT", "/shared-bucket/from-bob", b"x")
            .await,
        StatusCode::OK
    );
    assert_eq!(
        h.s3(&app_key, "GET", "/shared-bucket/from-bob", b"").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn reopening_a_project_returns_the_same_session_key() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;

    let first = h.session_key(&alice, &project).await;
    let second = h.session_key(&alice, &project).await;
    assert_eq!(first, second);

    // It ends when the sign-in it came from does.
    let parent = h
        .metadata
        .get_credential(&alice.access_key)
        .await
        .unwrap()
        .unwrap();
    let stored = h.metadata.get_credential(&first.0).await.unwrap().unwrap();
    assert_eq!(stored.expires_at, parent.expires_at);
    assert_eq!(stored.kind, CredentialKind::ProjectSession);
    assert!(!stored.admin);
}

#[tokio::test]
async fn a_session_key_cannot_use_the_console_api() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    for key in [
        h.session_key(&alice, &project).await,
        h.new_key(&alice, &project).await,
    ] {
        let uri = "/_console/v1/projects";
        let (status, _) = h.raw(signed("GET", uri, &key.0, &key.1, Vec::new())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn signing_out_ends_the_project_session_keys_opened_from_it() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let session = h.session_key(&alice, &project).await;
    let app_key = h.new_key(&alice, &project).await;
    assert_eq!(h.s3(&session, "GET", "/", b"").await, StatusCode::OK);

    h.ok(&alice, "DELETE", "/session", Value::Null).await;

    assert_eq!(h.s3(&session, "GET", "/", b"").await, StatusCode::FORBIDDEN);
    assert_eq!(
        h.s3(&app_key, "GET", "/", b"").await,
        StatusCode::OK,
        "app keys aren't tied to a sign-in"
    );
}

#[tokio::test]
async fn removing_a_member_cuts_off_their_session_but_not_the_apps() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;

    let bob_session = h.session_key(&bob, &project).await;
    let bobs_app_key = h.new_key(&bob, &project).await;
    assert_eq!(h.s3(&bob_session, "GET", "/", b"").await, StatusCode::OK);

    h.ok(
        &alice,
        "DELETE",
        &format!("/projects/{project}/members/{}", bob.id),
        Value::Null,
    )
    .await;

    assert_eq!(
        h.s3(&bob_session, "GET", "/", b"").await,
        StatusCode::FORBIDDEN
    );
    let (status, _) = h
        .call(
            &bob,
            "POST",
            &format!("/projects/{project}/session"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.s3(&bobs_app_key, "GET", "/", b"").await, StatusCode::OK);
}

#[tokio::test]
async fn disabling_a_user_cuts_off_their_sessions_but_not_the_apps() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    let bob_session = h.session_key(&bob, &project).await;
    let bobs_app_key = h.new_key(&bob, &project).await;

    h.metadata
        .set_user_access(
            bob.id,
            UserAccess {
                enabled: false,
                can_create_projects: true,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        h.s3(&bob_session, "GET", "/", b"").await,
        StatusCode::FORBIDDEN
    );
    let (status, body) = h.call(&bob, "GET", "/projects", Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "account_disabled");
    assert_eq!(h.s3(&bobs_app_key, "GET", "/", b"").await, StatusCode::OK);
}

#[tokio::test]
async fn disabling_a_project_stops_its_keys_and_enabling_it_keeps_their_own_state() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let project_id: loony_core::ProjectId = project.parse().unwrap();

    let session = h.session_key(&alice, &project).await;
    let on = h.new_key(&alice, &project).await;
    let off = h.new_key(&alice, &project).await;
    h.ok(
        &alice,
        "PATCH",
        &format!("/projects/{project}/keys/{}", off.0),
        json!({"enabled": false}),
    )
    .await;

    h.metadata
        .set_project_enabled(project_id, false)
        .await
        .unwrap();

    for key in [&session, &on, &off] {
        assert_eq!(h.s3(key, "GET", "/", b"").await, StatusCode::FORBIDDEN);
    }
    let (status, body) = h
        .call(
            &alice,
            "POST",
            &format!("/projects/{project}/keys"),
            json!({"name": "K"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "project_disabled");
    let (status, _) = h
        .call(
            &alice,
            "POST",
            &format!("/projects/{project}/session"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // They can still see what's there.
    let (status, _) = h
        .call(
            &alice,
            "GET",
            &format!("/projects/{project}/keys"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    h.metadata
        .set_project_enabled(project_id, true)
        .await
        .unwrap();

    assert_eq!(h.s3(&on, "GET", "/", b"").await, StatusCode::OK);
    assert_eq!(
        h.s3(&off, "GET", "/", b"").await,
        StatusCode::FORBIDDEN,
        "still off"
    );
    assert_eq!(
        h.s3(&session, "GET", "/", b"").await,
        StatusCode::FORBIDDEN,
        "ended for good"
    );
}

#[tokio::test]
async fn a_project_with_buckets_cannot_be_deleted() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let key = h.new_key(&alice, &project).await;
    let session = h.session_key(&alice, &project).await;
    assert_eq!(h.s3(&key, "PUT", "/keep-me", b"").await, StatusCode::OK);
    h.ok(
        &alice,
        "POST",
        &format!("/projects/{project}/invites"),
        json!({"email": "x@example.com", "role": "member"}),
    )
    .await;

    let (status, body) = h
        .call(
            &alice,
            "DELETE",
            &format!("/projects/{project}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "project_not_empty");
    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::OK);

    assert_eq!(
        h.s3(&key, "DELETE", "/keep-me", b"").await,
        StatusCode::NO_CONTENT
    );
    h.ok(
        &alice,
        "DELETE",
        &format!("/projects/{project}"),
        Value::Null,
    )
    .await;

    assert_ne!(h.s3(&key, "GET", "/", b"").await, StatusCode::OK);
    assert_ne!(h.s3(&session, "GET", "/", b"").await, StatusCode::OK);
    let id: loony_core::ProjectId = project.parse().unwrap();
    assert!(h.metadata.get_project(id).await.unwrap().is_none());
    assert!(h.metadata.list_members(id).await.unwrap().is_empty());
    assert!(h.metadata.list_invites().await.unwrap().is_empty());
    let leftovers: Vec<_> = h
        .metadata
        .list_credentials()
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.project_id == Some(id))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn a_secret_is_only_shown_when_the_key_is_created() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let (access_key, secret_key) = h.new_key(&alice, &project).await;

    let list = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/keys"),
            Value::Null,
        )
        .await;
    assert_eq!(list[0]["access_key"], access_key.as_str());
    assert_eq!(list[0]["name"], "An app");
    assert_eq!(list[0]["created_by"], alice.id.to_string());
    assert!(!list.to_string().contains(&secret_key));
    assert!(list[0].get("secret_key").is_none());

    let updated = h
        .ok(
            &alice,
            "PATCH",
            &format!("/projects/{project}/keys/{access_key}"),
            json!({"enabled": true}),
        )
        .await;
    assert!(!updated.to_string().contains(&secret_key));
}

#[tokio::test]
async fn keys_can_be_disabled_enabled_and_deleted() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let key = h.new_key(&alice, &project).await;
    let path = format!("/projects/{project}/keys/{}", key.0);

    let updated = h
        .ok(&alice, "PATCH", &path, json!({"enabled": false}))
        .await;
    assert_eq!(updated["enabled"], false);
    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::FORBIDDEN);

    h.ok(&alice, "PATCH", &path, json!({"enabled": true})).await;
    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::OK);

    h.ok(&alice, "DELETE", &path, Value::Null).await;
    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::FORBIDDEN);
    assert_eq!(
        h.call(&alice, "DELETE", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn sign_in_session_keys_cannot_be_managed_as_access_keys() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    let bob_session = h.session_key(&bob, &project).await;

    let path = format!("/projects/{project}/keys/{}", bob_session.0);
    assert_eq!(
        h.call(&alice, "DELETE", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    let list = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/keys"),
            Value::Null,
        )
        .await;
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_project_can_only_have_so_many_access_keys() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    for _ in 0..MAX_ACCESS_KEYS_PER_PROJECT {
        h.new_key(&alice, &project).await;
    }
    let (status, body) = h
        .call(
            &alice,
            "POST",
            &format!("/projects/{project}/keys"),
            json!({"name": "K"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "key_limit_reached");

    // Session keys don't count against it.
    h.session_key(&alice, &project).await;
}

#[tokio::test]
async fn using_an_access_key_records_when() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let key = h.new_key(&alice, &project).await;

    let list = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/keys"),
            Value::Null,
        )
        .await;
    assert!(list[0]["last_used_at"].is_null());

    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::OK);
    let mut first = None;
    for _ in 0..200 {
        first = h
            .metadata
            .get_credential(&key.0)
            .await
            .unwrap()
            .unwrap()
            .last_used_at;
        if first.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let first = first.expect("recorded soon after use");

    // Within the hour it isn't written again.
    assert_eq!(h.s3(&key, "GET", "/", b"").await, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let later = h
        .metadata
        .get_credential(&key.0)
        .await
        .unwrap()
        .unwrap()
        .last_used_at;
    assert_eq!(later, Some(first));

    let list = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/keys"),
            Value::Null,
        )
        .await;
    assert!(list[0]["last_used_at"].is_string());
}

#[tokio::test]
async fn the_last_owner_cannot_leave_or_be_demoted() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;

    let alice_path = format!("/projects/{project}/members/{}", alice.id);
    let bob_path = format!("/projects/{project}/members/{}", bob.id);
    let leave = format!("/projects/{project}/leave");

    let (status, body) = h.call(&alice, "POST", &leave, Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "last_owner");
    let (status, _) = h
        .call(&alice, "PATCH", &alice_path, json!({"role": "member"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = h.call(&alice, "DELETE", &alice_path, Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // With a second Owner, either can go.
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    let (status, _) = h.call(&alice, "POST", &leave, Value::Null).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a Member doesn't count as an Owner"
    );
    let updated = h
        .ok(&alice, "PATCH", &bob_path, json!({"role": "owner"}))
        .await;
    assert_eq!(updated["role"], "owner");
    h.ok(&alice, "POST", &leave, Value::Null).await;

    let (status, _) = h
        .call(&alice, "GET", &format!("/projects/{project}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.call(&bob, "POST", &leave, Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_member_can_leave() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    let session = h.session_key(&bob, &project).await;

    h.ok(
        &bob,
        "POST",
        &format!("/projects/{project}/leave"),
        Value::Null,
    )
    .await;

    assert_eq!(h.s3(&session, "GET", "/", b"").await, StatusCode::FORBIDDEN);
    assert_eq!(
        h.ok(&bob, "GET", "/projects", Value::Null).await["joined"],
        json!([])
    );
}

#[tokio::test]
async fn members_are_listed_with_their_details() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;

    let members = h
        .ok(
            &bob,
            "GET",
            &format!("/projects/{project}/members"),
            Value::Null,
        )
        .await;
    assert_eq!(members[0]["email"], "alice@example.com");
    assert_eq!(members[0]["role"], "owner");
    assert_eq!(members[1]["user_id"], bob.id.to_string());
    assert_eq!(members[1]["name"], "Name of bob@example.com");
    assert_eq!(members[1]["role"], "member");
}

#[tokio::test]
async fn an_accepted_invite_gives_the_invited_role() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("Bob@Example.com").await;
    let project = h.project(&alice, "P").await;

    let invite = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{project}/invites"),
            json!({"email": " BOB@example.com ", "role": "owner"}),
        )
        .await;
    assert_eq!(invite["email"], "bob@example.com");

    let id = invite["id"].as_str().unwrap();
    let joined = h
        .ok(&bob, "POST", &format!("/invites/{id}/accept"), Value::Null)
        .await;
    assert_eq!(joined["role"], "owner");
    assert_eq!(joined["created_by_me"], false);

    // It's used up.
    assert_eq!(
        h.call(&bob, "POST", &format!("/invites/{id}/accept"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let invites = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/invites"),
            Value::Null,
        )
        .await;
    assert_eq!(invites, json!([]));
}

#[tokio::test]
async fn an_invite_only_works_for_the_invited_verified_email() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let carol = h.person("carol@example.com").await;
    let project = h.project(&alice, "P").await;
    let invite = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{project}/invites"),
            json!({"email": "bob@example.com", "role": "owner"}),
        )
        .await;
    let id = invite["id"].as_str().unwrap();
    let project_id: loony_core::ProjectId = project.parse().unwrap();

    for action in ["accept", "decline"] {
        let (status, body) = h
            .call(
                &carol,
                "POST",
                &format!("/invites/{id}/{action}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}");
        assert_eq!(body["code"], "not_found");
    }

    // Someone claiming that address without having verified it gets nowhere.
    let claimant = h.person_with("bob@example.com", false, false).await;
    let (status, body) = h
        .call(
            &claimant,
            "POST",
            &format!("/invites/{id}/accept"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "email_not_verified");
    let list = h.ok(&claimant, "GET", "/projects", Value::Null).await;
    assert_eq!(
        list["invites"],
        json!([]),
        "unverified addresses aren't shown invites"
    );
    assert!(
        h.metadata
            .get_member(project_id, claimant.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.metadata
            .get_member(project_id, carol.id)
            .await
            .unwrap()
            .is_none()
    );

    // The invite survives all that for its real recipient.
    assert!(
        h.metadata
            .get_invite(id.parse().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn invites_can_be_declined_and_cancelled() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    let invites = format!("/projects/{project}/invites");
    let body = json!({"email": "bob@example.com", "role": "member"});

    let invite = h.ok(&alice, "POST", &invites, body.clone()).await;
    let id = invite["id"].as_str().unwrap();
    h.ok(&bob, "POST", &format!("/invites/{id}/decline"), Value::Null)
        .await;
    assert_eq!(h.ok(&alice, "GET", &invites, Value::Null).await, json!([]));
    assert_eq!(
        h.ok(&bob, "GET", "/projects", Value::Null).await["joined"],
        json!([])
    );

    let invite = h.ok(&alice, "POST", &invites, body).await;
    let id = invite["id"].as_str().unwrap();
    h.ok(&alice, "DELETE", &format!("{invites}/{id}"), Value::Null)
        .await;
    assert_eq!(
        h.call(&bob, "POST", &format!("/invites/{id}/accept"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_invite_cannot_be_cancelled_through_another_project() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let a = h.project(&alice, "A").await;
    let b = h.project(&bob, "B").await;
    let invite = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{a}/invites"),
            json!({"email": "x@example.com", "role": "member"}),
        )
        .await;
    let id = invite["id"].as_str().unwrap();

    let (status, _) = h
        .call(
            &bob,
            "DELETE",
            &format!("/projects/{b}/invites/{id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        h.metadata
            .get_invite(id.parse().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn pointless_invites_are_refused() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let invites = format!("/projects/{project}/invites");

    let body = json!({"email": "bob@example.com", "role": "member"});
    h.ok(&alice, "POST", &invites, body.clone()).await;
    let (status, response) = h
        .call(
            &alice,
            "POST",
            &invites,
            json!({"email": "BOB@EXAMPLE.COM", "role": "owner"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(response["code"], "invite_exists");

    let (status, response) = h
        .call(
            &alice,
            "POST",
            &invites,
            json!({"email": "Alice@Example.com", "role": "member"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(response["code"], "already_member");

    for email in [
        "",
        "no-at-sign",
        "@example.com",
        "a@nodot",
        "two words@example.com",
    ] {
        let (status, _) = h
            .call(
                &alice,
                "POST",
                &invites,
                json!({"email": email, "role": "member"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{email:?}");
    }
    let (status, _) = h
        .call(
            &alice,
            "POST",
            &invites,
            json!({"email": "c@example.com", "role": "emperor"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invites_expire() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    let project_id: loony_core::ProjectId = project.parse().unwrap();

    // The normal lifetime is a week.
    let fresh = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{project}/invites"),
            json!({"email": "c@example.com", "role": "member"}),
        )
        .await;
    let expires = OffsetDateTime::parse(
        fresh["expires_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let lifetime = expires - OffsetDateTime::now_utc();
    assert!(lifetime > time::Duration::days(6) && lifetime <= time::Duration::days(7));

    let expired = h
        .metadata
        .create_invite(CreateInvite {
            project_id,
            email: "bob@example.com".into(),
            role: ProjectRole::Member,
            invited_by: alice.id,
            expires_at: OffsetDateTime::now_utc() - time::Duration::seconds(1),
        })
        .await
        .unwrap();

    let (status, body) = h
        .call(
            &bob,
            "POST",
            &format!("/invites/{}/accept", expired.invite_id),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "invite_expired");
    assert_eq!(
        h.ok(&bob, "GET", "/projects", Value::Null).await["invites"],
        json!([])
    );
    let listed = h
        .ok(
            &alice,
            "GET",
            &format!("/projects/{project}/invites"),
            Value::Null,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "only the live one");

    // An expired one doesn't stop a new invite, which replaces it.
    let again = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{project}/invites"),
            json!({"email": "bob@example.com", "role": "member"}),
        )
        .await;
    h.ok(
        &bob,
        "POST",
        &format!("/invites/{}/accept", again["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    assert!(
        h.metadata
            .get_invite(expired.invite_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cleanup_removes_expired_invites() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let project = h.project(&alice, "P").await;
    let project_id: loony_core::ProjectId = project.parse().unwrap();
    let now = OffsetDateTime::now_utc();
    let invite = |email: &str, expires_at| CreateInvite {
        project_id,
        email: email.into(),
        role: ProjectRole::Member,
        invited_by: alice.id,
        expires_at,
    };
    let old = h
        .metadata
        .create_invite(invite("old@example.com", now - time::Duration::hours(1)))
        .await
        .unwrap();
    let live = h
        .metadata
        .create_invite(invite("live@example.com", now + time::Duration::hours(1)))
        .await
        .unwrap();

    assert_eq!(
        delete_expired_credentials(h.metadata.as_ref(), now)
            .await
            .unwrap(),
        1
    );
    assert!(
        h.metadata
            .get_invite(old.invite_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.metadata
            .get_invite(live.invite_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn changes_are_audited_without_any_secrets() {
    let h = Harness::new().await;
    let alice = h.person("alice@example.com").await;
    let bob = h.person("bob@example.com").await;
    let project = h.project(&alice, "P").await;
    h.ok(
        &alice,
        "PATCH",
        &format!("/projects/{project}"),
        json!({"name": "Renamed"}),
    )
    .await;
    let (access_key, secret_key) = h.new_key(&alice, &project).await;
    let key_path = format!("/projects/{project}/keys/{access_key}");
    h.ok(&alice, "PATCH", &key_path, json!({"enabled": false}))
        .await;
    h.ok(&alice, "PATCH", &key_path, json!({"enabled": true}))
        .await;
    h.ok(&alice, "DELETE", &key_path, Value::Null).await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    let member = format!("/projects/{project}/members/{}", bob.id);
    h.ok(&alice, "PATCH", &member, json!({"role": "owner"}))
        .await;
    h.ok(&alice, "PATCH", &member, json!({"role": "member"}))
        .await;
    let invite = h
        .ok(
            &alice,
            "POST",
            &format!("/projects/{project}/invites"),
            json!({"email": "x@example.com", "role": "member"}),
        )
        .await;
    h.ok(
        &alice,
        "DELETE",
        &format!(
            "/projects/{project}/invites/{}",
            invite["id"].as_str().unwrap()
        ),
        Value::Null,
    )
    .await;
    h.ok(&alice, "DELETE", &member, Value::Null).await;
    h.join(&alice, &project, &bob, "bob@example.com", "member")
        .await;
    h.ok(
        &bob,
        "POST",
        &format!("/projects/{project}/leave"),
        Value::Null,
    )
    .await;
    h.ok(
        &alice,
        "DELETE",
        &format!("/projects/{project}"),
        Value::Null,
    )
    .await;
    // And something refused.
    let (status, _) = h
        .call(&bob, "POST", "/projects/not-a-real-action", Value::Null)
        .await;
    assert!(status.is_client_error());
    let denied = h
        .call(&alice, "POST", "/projects", json!({"name": ""}))
        .await;
    assert_eq!(denied.0, StatusCode::BAD_REQUEST);

    let log = std::fs::read_to_string(&h.audit_path).unwrap();
    for action in [
        "project.create",
        "project.rename",
        "project.delete",
        "key.create",
        "key.disable",
        "key.enable",
        "key.delete",
        "invite.create",
        "invite.cancel",
        "invite.accept",
        "member.update",
        "member.remove",
        "member.leave",
    ] {
        assert!(
            log.contains(&format!("\"action\":\"{action}\"")),
            "missing {action}"
        );
    }
    assert!(log.contains(&format!("user:{}", alice.id)));
    assert!(log.contains("\"outcome\":\"denied\""));
    assert!(
        !log.contains(&secret_key),
        "a key's secret reached the audit log"
    );
    assert!(!log.contains(&alice.secret_key));
}
