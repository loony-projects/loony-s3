//! Drives the admin router end to end over a real single-voter Raft store: real SigV4
//! signatures, real state machine, no mocks.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use loony_admin::{AdminState, AuditLog, build_router};
use loony_core::{
    BucketName, DurabilityPolicy, ETag, NodeId, NodeState, ObjectId, ObjectKey, ObjectManifest,
    OwnerId, PartManifest, ShardId, ShardLocation, Stripe, VersionId, VolumeId,
};
use loony_metadata::{CreateBucket, Credential, MetadataStore, RaftMetadataStore, RegisterNode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tower::ServiceExt;

const REGION: &str = "us-east-1";
const ADMIN_KEY: &str = "ADMINKEY";
const ADMIN_SECRET: &str = "admin-secret-value";
const USER_KEY: &str = "USERKEY";
const USER_SECRET: &str = "user-secret-value";

struct App {
    router: Router,
    store: Arc<RaftMetadataStore>,
    node_id: NodeId,
    user_owner: OwnerId,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let node_id = NodeId::new();
        let store = Arc::new(
            RaftMetadataStore::open_single_node(node_id, "self:9100".into(), dir.path().join("meta.redb"))
                .await
                .unwrap(),
        );
        for _ in 0..200 {
            if store.raft_status().leader == Some(node_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let user_owner = OwnerId::new();
        for (access_key, secret_key, owner_id, admin) in [
            (ADMIN_KEY, ADMIN_SECRET, OwnerId::new(), true),
            (USER_KEY, USER_SECRET, user_owner, false),
        ] {
            store
                .put_credential(Credential {
                    access_key: access_key.into(),
                    secret_key: secret_key.into(),
                    owner_id,
                    enabled: true,
                    admin,
                    kind: Default::default(),
                    user_id: None,
                    expires_at: None,
                    project_id: None,
                    name: None,
                    session_of: None,
                    last_used_at: None,
                    created_at: OffsetDateTime::now_utc(),
                })
                .await
                .unwrap();
        }

        let audit_path = dir.path().join("audit.jsonl");
        let router = build_router(AdminState {
            metadata: store.clone(),
            audit: Arc::new(AuditLog::new(audit_path.clone())),
            region: REGION.into(),
            node_id,
            mode: "standalone",
            started_at: OffsetDateTime::now_utc(),
        });
        Self { router, store, node_id, user_owner, audit_path, _dir: dir }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn admin(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let body = body.map_or_else(Vec::new, |b| serde_json::to_vec(&b).unwrap());
        self.send(signed(method, uri, body, ADMIN_KEY, ADMIN_SECRET)).await
    }

    async fn add_peer(&self, state: NodeState) -> NodeId {
        let node_id = NodeId::new();
        self.store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "127.0.0.1:1".into(),
                failure_domain: vec![],
                volumes: vec![VolumeId::new()],
            })
            .await
            .unwrap();
        if state != NodeState::Joining {
            self.store.update_node_state(node_id, state).await.unwrap();
        }
        node_id
    }

    async fn put_object(&self, owner_id: OwnerId, bucket: &str, key: &str, size: u64, on_node: NodeId) {
        let name = BucketName::parse(bucket).unwrap();
        let bucket = match self.store.get_bucket(&name).await.unwrap() {
            Some(bucket) => bucket,
            None => self
                .store
                .create_bucket(CreateBucket { name, owner_id, region: REGION.into() })
                .await
                .unwrap(),
        };
        self.store
            .commit_manifest(ObjectManifest {
                object_id: ObjectId::new(),
                bucket_id: bucket.bucket_id,
                key: ObjectKey::parse(key).unwrap(),
                version_id: VersionId::new(),
                size,
                etag: ETag::from_md5([1; 16]),
                sha256: [2; 32],
                content_type: "text/plain".into(),
                user_metadata: BTreeMap::new(),
                created_at: OffsetDateTime::now_utc(),
                delete_marker: false,
                parts: vec![PartManifest {
                    part_number: 1,
                    offset: 0,
                    size,
                    etag_md5: [1; 16],
                    stripes: vec![Stripe {
                        stripe_index: 0,
                        stripe_offset: 0,
                        stripe_len: size,
                        durability: DurabilityPolicy::Replicated { n: 1 },
                        shards: vec![ShardLocation {
                            shard_index: 0,
                            node_id: on_node,
                            volume_id: VolumeId::new(),
                            shard_id: ShardId::new(),
                            size,
                            checksum: [3; 32],
                            generation: 0,
                        }],
                    }],
                }],
            })
            .await
            .unwrap();
    }

    fn audit_contents(&self) -> String {
        std::fs::read_to_string(&self.audit_path).unwrap_or_default()
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn signed(method: &str, uri: &str, body: Vec<u8>, access_key: &str, secret_key: &str) -> Request<Body> {
    let payload_hash = sha256_hex(&body);
    signed_with_hash(method, uri, body, &payload_hash, access_key, secret_key)
}

fn signed_with_hash(
    method: &str,
    uri: &str,
    body: Vec<u8>,
    payload_hash: &str,
    access_key: &str,
    secret_key: &str,
) -> Request<Body> {
    let amz_date = loony_auth::amz_date_now();
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
    let headers = vec![
        ("host".to_string(), "localhost".to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];
    let signed_headers: Vec<String> = headers.iter().map(|(k, _)| k.clone()).collect();
    let authorization = loony_auth::sign_header_auth(
        method, path, query, &headers, &signed_headers, payload_hash, access_key, secret_key, REGION, &amz_date,
    );
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in &headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, authorization)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn only_a_correctly_signed_enabled_admin_key_gets_in() {
    let app = App::new().await;
    let uri = "/admin/v1/overview";

    let anonymous = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(app.send(anonymous).await.0, StatusCode::UNAUTHORIZED);

    let wrong_secret = signed("GET", uri, vec![], ADMIN_KEY, "not-the-secret");
    assert_eq!(app.send(wrong_secret).await.0, StatusCode::UNAUTHORIZED);

    let non_admin = signed("GET", uri, vec![], USER_KEY, USER_SECRET);
    assert_eq!(app.send(non_admin).await.0, StatusCode::FORBIDDEN);

    assert_eq!(app.admin("GET", uri, None).await.0, StatusCode::OK);

    // Rejections are on the record, and never include what was presented as a secret.
    let audit = app.audit_contents();
    assert!(audit.contains(USER_KEY) && audit.contains("not an admin key"));
    assert!(!audit.contains("not-the-secret"));
}

#[tokio::test]
async fn a_tampered_body_is_rejected() {
    let app = App::new().await;
    let declared = sha256_hex(br#"{"enabled":true}"#);
    let request = signed_with_hash(
        "PATCH",
        &format!("/admin/v1/credentials/{USER_KEY}"),
        br#"{"enabled":false}"#.to_vec(),
        &declared,
        ADMIN_KEY,
        ADMIN_SECRET,
    );
    assert_eq!(app.send(request).await.0, StatusCode::BAD_REQUEST);
    assert!(app.store.get_credential(USER_KEY).await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn presigned_style_query_auth_is_not_accepted() {
    let app = App::new().await;
    let request = Request::builder()
        .uri("/admin/v1/overview?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cors_only_admits_local_origins() {
    let app = App::new().await;
    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/admin/v1/overview")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .unwrap()
    };
    let allowed = app.router.clone().oneshot(preflight("http://localhost:5173")).await.unwrap();
    assert_eq!(
        allowed.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "http://localhost:5173"
    );
    let refused = app.router.clone().oneshot(preflight("https://s3.lovegood.in")).await.unwrap();
    assert!(refused.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn a_new_key_shows_its_secret_once_and_is_never_an_admin() {
    let app = App::new().await;
    let (status, created) = app.admin("POST", "/admin/v1/credentials", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let access_key = created["access_key"].as_str().unwrap();
    let secret_key = created["secret_key"].as_str().unwrap();
    assert_eq!(secret_key.len(), 64);
    assert_eq!(created["admin"], false);

    // It really works for LS3 (signature verifies), but not here.
    let attempt = signed("GET", "/admin/v1/overview", vec![], access_key, secret_key);
    assert_eq!(app.send(attempt).await.0, StatusCode::FORBIDDEN);

    let (_, listed) = app.admin("GET", "/admin/v1/credentials", None).await;
    let listed_text = listed.to_string();
    for secret in [secret_key, ADMIN_SECRET, USER_SECRET] {
        assert!(!listed_text.contains(secret), "listing leaked a secret");
        assert!(!app.audit_contents().contains(secret), "audit log leaked a secret");
    }
    assert!(listed.as_array().unwrap().iter().all(|c| c.get("secret_key").is_none()));
}

#[tokio::test]
async fn keys_can_join_an_existing_owner_but_not_an_unknown_one() {
    let app = App::new().await;
    let body = json!({ "owner_id": app.user_owner });
    let (status, created) = app.admin("POST", "/admin/v1/credentials", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["owner_id"], json!(app.user_owner));

    let body = json!({ "owner_id": OwnerId::new() });
    let (status, _) = app.admin("POST", "/admin/v1/credentials", Some(body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn you_cannot_lock_yourself_out() {
    let app = App::new().await;
    let uri = format!("/admin/v1/credentials/{ADMIN_KEY}");
    let (status, _) = app.admin("PATCH", &uri, Some(json!({ "enabled": false }))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = app.admin("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(app.store.get_credential(ADMIN_KEY).await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn a_disabled_key_stops_working_and_can_be_reenabled() {
    let app = App::new().await;
    let uri = format!("/admin/v1/credentials/{USER_KEY}");
    let (status, body) = app.admin("PATCH", &uri, Some(json!({ "enabled": false }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert!(!app.store.get_credential(USER_KEY).await.unwrap().unwrap().enabled);

    app.admin("PATCH", &uri, Some(json!({ "enabled": true }))).await;
    assert!(app.store.get_credential(USER_KEY).await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn deleting_the_only_key_of_an_owner_with_buckets_is_refused() {
    let app = App::new().await;
    app.put_object(app.user_owner, "user-bucket", "a.txt", 5, app.node_id).await;
    let uri = format!("/admin/v1/credentials/{USER_KEY}");

    let (status, body) = app.admin("DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("1 bucket"));

    // With a second key for that owner, the data stays reachable, so it's fine.
    let body = json!({ "owner_id": app.user_owner });
    app.admin("POST", "/admin/v1/credentials", Some(body)).await;
    assert_eq!(app.admin("DELETE", &uri, None).await.0, StatusCode::NO_CONTENT);
    assert!(app.store.get_credential(USER_KEY).await.unwrap().is_none());
}

#[tokio::test]
async fn the_metadata_voter_cannot_be_drained_or_removed() {
    let app = App::new().await;
    // The leader registers itself too in real cluster mode.
    app.store
        .register_node(RegisterNode {
            node_id: app.node_id,
            advertised_address: "self:9100".into(),
            failure_domain: vec![],
            volumes: vec![],
        })
        .await
        .unwrap();
    let uri = format!("/admin/v1/nodes/{}/drain", app.node_id);
    let (status, body) = app.admin("POST", &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("voter"));
}

#[tokio::test]
async fn node_lifecycle_guards_data_at_every_step() {
    let app = App::new().await;
    let peer = app.add_peer(NodeState::Healthy).await;
    let (drain, resume, remove) = (
        format!("/admin/v1/nodes/{peer}/drain"),
        format!("/admin/v1/nodes/{peer}/resume"),
        format!("/admin/v1/nodes/{peer}/remove"),
    );
    let state = async || {
        app.store.list_nodes().await.unwrap().into_iter().find(|n| n.node_id == peer).unwrap().state
    };

    // Must be drained first, and resume only applies to a draining node.
    assert_eq!(app.admin("POST", &remove, None).await.0, StatusCode::CONFLICT);
    assert_eq!(app.admin("POST", &resume, None).await.0, StatusCode::CONFLICT);

    assert_eq!(app.admin("POST", &drain, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(state().await, NodeState::Draining);

    // Resume hands it back to the heartbeat loop by way of Joining.
    assert_eq!(app.admin("POST", &resume, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(state().await, NodeState::Joining);
    assert_eq!(app.admin("POST", &drain, None).await.0, StatusCode::NO_CONTENT);

    // It holds the only copy of something, so removing it would lose data.
    app.put_object(app.user_owner, "bucket-on-peer", "a.txt", 5, peer).await;
    let (status, body) = app.admin("POST", &remove, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"].as_str().unwrap().contains("1 shard"));
    assert_eq!(state().await, NodeState::Draining);

    let (_, nodes) = app.admin("GET", "/admin/v1/nodes", None).await;
    assert_eq!(nodes[0]["shards"], 1);

    // Once nothing is left on it, removal goes through.
    let bucket = app
        .store
        .get_bucket(&BucketName::parse("bucket-on-peer").unwrap())
        .await
        .unwrap()
        .unwrap();
    app.store
        .tombstone_object(bucket.bucket_id, &ObjectKey::parse("a.txt").unwrap())
        .await
        .unwrap();
    assert_eq!(app.admin("POST", &remove, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(state().await, NodeState::Removed);

    // Final: no way back, and repeating it is harmless.
    assert_eq!(app.admin("POST", &drain, None).await.0, StatusCode::CONFLICT);
    assert_eq!(app.admin("POST", &remove, None).await.0, StatusCode::NO_CONTENT);

    let unknown = format!("/admin/v1/nodes/{}/drain", NodeId::new());
    assert_eq!(app.admin("POST", &unknown, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admins_see_every_owners_buckets_and_each_browse_is_recorded() {
    let app = App::new().await;
    let other_owner = OwnerId::new();
    app.put_object(app.user_owner, "alpha-bucket", "docs/a.txt", 100, app.node_id).await;
    app.put_object(app.user_owner, "alpha-bucket", "docs/b.txt", 50, app.node_id).await;
    app.put_object(app.user_owner, "alpha-bucket", "root.txt", 1, app.node_id).await;
    app.put_object(other_owner, "beta-bucket", "c.txt", 7, app.node_id).await;

    let (_, buckets) = app.admin("GET", "/admin/v1/buckets", None).await;
    assert_eq!(buckets[0]["name"], "alpha-bucket");
    assert_eq!(buckets[0]["object_count"], 3);
    assert_eq!(buckets[0]["total_bytes"], 151);
    assert_eq!(buckets[1]["owner_id"], json!(other_owner));

    let (_, overview) = app.admin("GET", "/admin/v1/overview", None).await;
    assert_eq!(overview["totals"]["buckets"], 2);
    assert_eq!(overview["totals"]["objects"], 4);
    assert_eq!(overview["totals"]["bytes"], 158);
    assert_eq!(overview["totals"]["owners"], 3);
    assert_eq!(overview["mode"], "standalone");

    let (_, top) = app.admin("GET", "/admin/v1/buckets/alpha-bucket/objects", None).await;
    assert_eq!(top["folders"], json!(["docs/"]));
    assert_eq!(top["objects"][0]["key"], "root.txt");

    let (_, docs) = app.admin("GET", "/admin/v1/buckets/alpha-bucket/objects?prefix=docs%2F", None).await;
    assert_eq!(docs["objects"].as_array().unwrap().len(), 2);

    let (status, _) = app.admin("GET", "/admin/v1/buckets/no-such-bucket/objects", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, audit) = app.admin("GET", "/admin/v1/audit?limit=3", None).await;
    assert_eq!(audit[0]["outcome"], "denied"); // the missing bucket, newest first
    assert_eq!(audit[1]["action"], "bucket.browse");
    assert_eq!(audit[1]["target"], "alpha-bucket/docs/");
    assert_eq!(audit[1]["actor"], ADMIN_KEY);
}

#[tokio::test]
async fn an_old_owner_can_become_a_project_but_the_root_owner_cannot() {
    let app = App::new().await;
    let user = app
        .store
        .record_user_sign_in(loony_metadata::UserSignIn {
            issuer: "https://issuer.example".into(),
            subject: "alice".into(),
            email: Some("alice@example.com".into()),
            email_verified: true,
            name: None,
        })
        .await
        .unwrap();

    let (status, created) = app.admin("POST", "/admin/v1/credentials", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let owner = created["owner_id"].as_str().unwrap().to_string();

    let adopt = |owner: &str, name: &str| {
        json!({"owner_id": owner, "name": name, "user_id": user.user_id})
    };
    let path = "/admin/v1/projects/adopt-owner";

    let (status, _) = app.admin("POST", path, Some(adopt(&owner, "  "))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, project) = app.admin("POST", path, Some(adopt(&owner, "Imported"))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(project["name"], "Imported");

    let (status, _) = app.admin("POST", path, Some(adopt(&owner, "Again"))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Its keys are managed in the project from now on.
    let (status, _) = app
        .admin("POST", "/admin/v1/credentials", Some(json!({"owner_id": owner})))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let keys = app.store.list_credentials().await.unwrap();
    let root = keys.iter().find(|key| key.admin).unwrap().owner_id.to_string();
    let (status, _) = app.admin("POST", path, Some(adopt(&root, "Root"))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = app.admin("POST", path, Some(adopt(&OwnerId::new().to_string(), "None"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert!(app.audit_contents().contains("project.adopt_owner"));
}

#[tokio::test]
async fn sign_in_session_keys_are_left_out_of_the_key_list_but_project_keys_are_shown() {
    let app = App::new().await;
    let key = |access_key: &str, kind| Credential {
        access_key: access_key.into(),
        secret_key: "secret".into(),
        owner_id: OwnerId::new(),
        enabled: true,
        admin: false,
        kind,
        user_id: None,
        expires_at: None,
        project_id: None,
        name: None,
        session_of: None,
        last_used_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    use loony_metadata::CredentialKind::{ProjectAccessKey, ProjectSession, UserSession};
    app.store.put_credential(key("USERSESSION", UserSession)).await.unwrap();
    app.store.put_credential(key("PROJSESSION", ProjectSession)).await.unwrap();
    app.store.put_credential(key("PROJKEY", ProjectAccessKey)).await.unwrap();

    let (status, list) = app.admin("GET", "/admin/v1/credentials", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = list.to_string();
    assert!(!listed.contains("USERSESSION"));
    assert!(!listed.contains("PROJSESSION"));
    assert!(listed.contains("PROJKEY"));
}

#[tokio::test]
async fn users_can_be_listed_approved_and_disabled() {
    let app = App::new().await;
    let user = app
        .store
        .record_user_sign_in(loony_metadata::UserSignIn {
            issuer: "https://issuer.example".into(),
            subject: "alice".into(),
            email: Some("alice@example.com".into()),
            email_verified: true,
            name: Some("Alice".into()),
        })
        .await
        .unwrap();

    let (status, list) = app.admin("GET", "/admin/v1/users", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["email"], "alice@example.com");
    assert_eq!(list[0]["can_create_projects"], false);
    assert_eq!(list[0]["enabled"], true);

    let path = format!("/admin/v1/users/{}", user.user_id);
    let (status, updated) = app.admin("PATCH", &path, Some(json!({"can_create_projects": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["can_create_projects"], true);
    assert_eq!(updated["enabled"], true, "what wasn't mentioned stays the same");

    let (status, updated) = app.admin("PATCH", &path, Some(json!({"enabled": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["can_create_projects"], true);

    let unknown = format!("/admin/v1/users/{}", loony_core::UserId::new());
    let (status, _) = app.admin("PATCH", &unknown, Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert!(app.audit_contents().contains("user.update"));
}

#[tokio::test]
async fn projects_are_listed_with_their_members_and_usage_and_can_be_disabled() {
    let app = App::new().await;
    let alice = app
        .store
        .record_user_sign_in(loony_metadata::UserSignIn {
            issuer: "https://issuer.example".into(),
            subject: "alice".into(),
            email: Some("alice@example.com".into()),
            email_verified: true,
            name: Some("Alice".into()),
        })
        .await
        .unwrap();
    let project = app
        .store
        .create_project(loony_metadata::CreateProject {
            name: "Website".into(),
            created_by: alice.user_id,
            max_created: 5,
        })
        .await
        .unwrap();
    let node = app.add_peer(NodeState::Healthy).await;
    app.put_object(project.owner_id, "website-assets", "logo.png", 1000, node).await;
    app.put_object(project.owner_id, "website-assets", "hero.png", 500, node).await;
    let key = |access_key: &str, kind| Credential {
        access_key: access_key.into(),
        secret_key: "secret".into(),
        owner_id: project.owner_id,
        enabled: true,
        admin: false,
        kind,
        user_id: Some(alice.user_id),
        expires_at: None,
        project_id: Some(project.project_id),
        name: Some("App".into()),
        session_of: None,
        last_used_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    use loony_metadata::CredentialKind::{ProjectAccessKey, ProjectSession};
    app.store.put_project_credential(key("APPKEY", ProjectAccessKey), 10).await.unwrap();
    app.store.put_project_credential(key("SESSIONKEY", ProjectSession), 10).await.unwrap();

    let (status, list) = app.admin("GET", "/admin/v1/projects", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["name"], "Website");
    assert_eq!(list[0]["enabled"], true);
    assert_eq!(list[0]["members"][0]["email"], "alice@example.com");
    assert_eq!(list[0]["members"][0]["role"], "owner");
    assert_eq!(list[0]["access_keys"], 1, "session keys aren't access keys");
    assert_eq!(list[0]["buckets"], 1);
    assert_eq!(list[0]["objects"], 2);
    assert_eq!(list[0]["bytes"], 1500);

    let (_, keys) = app.admin("GET", "/admin/v1/credentials", None).await;
    let listed = keys.as_array().unwrap().iter().find(|k| k["access_key"] == "APPKEY").unwrap();
    assert_eq!(listed["name"], "App");
    assert_eq!(listed["project_id"], project.project_id.to_string());

    let path = format!("/admin/v1/projects/{}", project.project_id);
    let (status, updated) = app.admin("PATCH", &path, Some(json!({"enabled": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["enabled"], false);
    assert!(app.store.get_credential("SESSIONKEY").await.unwrap().is_none());
    assert!(app.store.get_credential("APPKEY").await.unwrap().is_some());

    let (status, updated) = app.admin("PATCH", &path, Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["enabled"], true);

    let unknown = format!("/admin/v1/projects/{}", loony_core::ProjectId::new());
    let (status, _) = app.admin("PATCH", &unknown, Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let audit = app.audit_contents();
    assert!(audit.contains("project.disable") && audit.contains("project.enable"));
}

#[tokio::test]
async fn only_owners_outside_projects_can_be_adopted() {
    let app = App::new().await;
    let alice = app
        .store
        .record_user_sign_in(loony_metadata::UserSignIn {
            issuer: "https://issuer.example".into(),
            subject: "alice".into(),
            email: Some("alice@example.com".into()),
            email_verified: true,
            name: None,
        })
        .await
        .unwrap();
    let project = app
        .store
        .create_project(loony_metadata::CreateProject {
            name: "Existing".into(),
            created_by: alice.user_id,
            max_created: 5,
        })
        .await
        .unwrap();

    let (_, created) = app.admin("POST", "/admin/v1/credentials", Some(json!({}))).await;
    let old_owner = created["owner_id"].as_str().unwrap().to_string();

    // A sign-in's session key has its own owner, which is nothing to adopt.
    let session_owner = OwnerId::new();
    app.store
        .put_credential(Credential {
            access_key: "SESSION".into(),
            secret_key: "secret".into(),
            owner_id: session_owner,
            enabled: true,
            admin: false,
            kind: loony_metadata::CredentialKind::UserSession,
            user_id: Some(alice.user_id),
            expires_at: None,
            project_id: None,
            name: None,
            session_of: None,
            last_used_at: None,
            created_at: OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();

    let (status, owners) = app.admin("GET", "/admin/v1/projects/adoptable-owners", None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = owners.as_array().unwrap().iter().map(|o| o["owner_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&old_owner.as_str()));
    assert!(!ids.contains(&project.owner_id.to_string().as_str()), "already a project");
    assert!(!ids.contains(&session_owner.to_string().as_str()), "only a sign-in");
    let credentials = app.store.list_credentials().await.unwrap();
    let admin_owner = credentials.iter().find(|c| c.admin).unwrap().owner_id.to_string();
    assert!(!ids.contains(&admin_owner.as_str()), "holds an admin key");
    let listed = owners.as_array().unwrap().iter().find(|o| o["owner_id"] == old_owner.as_str()).unwrap();
    assert_eq!(listed["access_keys"], 1);

    let (status, _) = app
        .admin(
            "POST",
            "/admin/v1/projects/adopt-owner",
            Some(json!({"owner_id": old_owner, "name": "Moved", "user_id": alice.user_id})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, owners) = app.admin("GET", "/admin/v1/projects/adoptable-owners", None).await;
    assert!(!owners.to_string().contains(&old_owner), "it's a project now");
}
