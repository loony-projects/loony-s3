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
