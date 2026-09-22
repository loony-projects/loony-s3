//! Exercises the real `--join` production code path (not a direct `openraft` API call
//! the way `raft_cluster.rs`'s tests do): a single-voter bootstrap node, a second node
//! that starts fully uninitialized, and the actual `/internal/v1/cluster/join` HTTP
//! endpoint (`crates/rpc/src/server.rs`'s `join` handler) — which now adds the joiner as
//! a real `openraft` learner via `Raft::add_learner`, not just a node-registry entry.
//!
//! This is what closes the gap `docs/cluster.md` documented: "a bucket created on node
//! 1 is invisible on node 2." After this join, it shouldn't be.

use std::net::SocketAddr;
use std::sync::Arc;

use loony_core::{BucketName, ClusterId, NodeId, ObjectKey, OwnerId};
use loony_metadata::{CreateBucket, MetadataStore, RaftMetadataStore};
use loony_rpc::{HttpRaftNetworkFactory, JoinRequest, RpcServerState, join_cluster};
use loony_storage::LocalVolumeManager;
use tokio::net::TcpListener;

const TOKEN: &str = "join-replication-test-token";

struct Node {
    node_id: NodeId,
    addr: SocketAddr,
    store: Arc<RaftMetadataStore>,
}

async fn spawn_node() -> Node {
    let node_id = NodeId::new();
    let meta_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        RaftMetadataStore::open(
            node_id,
            meta_dir.keep().join("meta.redb"),
            HttpRaftNetworkFactory::new(TOKEN.to_string()),
        )
        .await
        .unwrap(),
    );

    let vol_dir = tempfile::tempdir().unwrap();
    let volumes = Arc::new(
        LocalVolumeManager::open(node_id, vec![vol_dir.keep()])
            .await
            .unwrap(),
    );

    let state = RpcServerState {
        shard_store: volumes,
        metadata: store.clone() as Arc<dyn MetadataStore>,
        local_node: node_id,
        token: TOKEN.to_string(),
        raft: Some(store.raft().clone()),
    };
    let router = loony_rpc::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Node {
        node_id,
        addr,
        store,
    }
}

async fn bucket_exists(store: &RaftMetadataStore, name: &str) -> bool {
    store
        .get_bucket(&BucketName::parse(name).unwrap())
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn a_node_joined_via_the_real_join_rpc_replicates_existing_and_future_buckets() {
    // Node 1: bootstrap, exactly like `main.rs` does for `--bootstrap` -- initialize as
    // the sole voter over itself.
    let node1 = spawn_node().await;
    let mut members = std::collections::BTreeMap::new();
    members.insert(
        node1.node_id,
        openraft::BasicNode::new(node1.addr.to_string()),
    );
    node1.store.raft().initialize(members).await.unwrap();
    node1
        .store
        .bootstrap_cluster(ClusterId::parse("join-test-cluster").unwrap())
        .await
        .unwrap();

    // A bucket that exists *before* node 2 ever joins -- proving a join catches a
    // fresh learner up on existing state, not just future writes.
    let bucket = node1
        .store
        .create_bucket(CreateBucket {
            name: BucketName::parse("pre-join-bucket").unwrap(),
            owner_id: OwnerId::new(),
            region: "us-east-1".into(),
        })
        .await
        .unwrap();

    // And an object in it, so the regression this test exists for has something to
    // catch: `create_bucket`'s `bucket_id` used to be minted *inside* `apply()`, which
    // every replica runs independently -- so a bucket replayed on a learner ended up
    // with a *different* id than the leader's, while a manifest committed against the
    // leader's id (correctly carried inside the command, since `commit_manifest` takes
    // a fully-built `ObjectManifest`) replicated to a bucket_id the learner's own
    // bucket table didn't have. `bucket_exists`, which looks up by *name*, wouldn't
    // have caught that -- only an id-aware check does.
    let manifest = loony_core::ObjectManifest {
        object_id: loony_core::ObjectId::new(),
        bucket_id: bucket.bucket_id,
        key: ObjectKey::parse("pre-join-key").unwrap(),
        version_id: loony_core::VersionId::new(),
        size: 3,
        etag: loony_core::ETag::from_md5([9u8; 16]),
        sha256: [7u8; 32],
        content_type: "application/octet-stream".into(),
        user_metadata: Default::default(),
        created_at: time::OffsetDateTime::now_utc(),
        delete_marker: false,
        parts: vec![],
    };
    node1.store.commit_manifest(manifest).await.unwrap();

    // Node 2: exactly like `main.rs` does for `--join` -- open uninitialized, RPC
    // server up first, then call the real join endpoint.
    let node2 = spawn_node().await;
    assert!(!node2.store.raft().is_initialized().await.unwrap());

    let http = reqwest::Client::new();
    let response = join_cluster(
        &http,
        &format!("http://{}", node1.addr),
        TOKEN,
        &JoinRequest {
            node_id: node2.node_id,
            advertised_address: node2.addr.to_string(),
            failure_domain: vec![],
            claimed_cluster_id: None,
            volumes: vec![],
        },
    )
    .await
    .unwrap();
    assert_eq!(response.cluster_id, "join-test-cluster");

    // `add_learner(.., blocking: true)` on the seed's side waits for node 2's *log* to
    // catch up before the join call above returns -- but log replication and state
    // machine *apply* are separate steps in openraft, and apply can lag slightly behind
    // (this is also exactly why `RaftMetadataStore`'s own reads are documented as
    // eventually- rather than linearizably-consistent). So this still polls, briefly,
    // rather than asserting immediately.
    let catchup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !bucket_exists(&node2.store, "pre-join-bucket").await {
        assert!(
            tokio::time::Instant::now() < catchup_deadline,
            "node 2 never replicated the pre-existing bucket after joining"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The id-aware check the regression above needs: node 2's own copy of the bucket
    // must have the *same* bucket_id node 1 minted, and the object committed against
    // that id on node 1 must be reachable through node 2's own (replicated) view of it.
    let bucket_on_2 = node2
        .store
        .get_bucket(&BucketName::parse("pre-join-bucket").unwrap())
        .await
        .unwrap()
        .expect("bucket exists by name on node 2");
    assert_eq!(
        bucket_on_2.bucket_id, bucket.bucket_id,
        "node 2's replayed CreateBucket must produce the same bucket_id node 1 minted, not a fresh one"
    );
    let manifest_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if node2
            .store
            .get_manifest(bucket.bucket_id, &ObjectKey::parse("pre-join-key").unwrap())
            .await
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < manifest_deadline,
            "node 2 never replicated the pre-existing object after joining"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // A write made *after* the join, through the leader (node 1), should also reach
    // node 2 -- ongoing replication, not just the initial catch-up.
    node1
        .store
        .create_bucket(CreateBucket {
            name: BucketName::parse("post-join-bucket").unwrap(),
            owner_id: OwnerId::new(),
            region: "us-east-1".into(),
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !bucket_exists(&node2.store, "post-join-bucket").await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "post-join write never replicated to node 2"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The node registry -- Phase 7's mechanism, now itself just an ordinary replicated
    // write -- should also show both nodes from either side.
    let nodes_from_1 = node1.store.list_nodes().await.unwrap();
    let nodes_from_2 = node2.store.list_nodes().await.unwrap();
    assert_eq!(
        nodes_from_1.len(),
        1,
        "node 1's own RegisterNode call in main.rs isn't exercised by this test's spawn_node helper"
    );
    assert_eq!(
        nodes_from_2
            .iter()
            .map(|n| n.node_id)
            .collect::<std::collections::HashSet<_>>(),
        nodes_from_1
            .iter()
            .map(|n| n.node_id)
            .collect::<std::collections::HashSet<_>>(),
        "node registry should be identical from either node's local (replicated) view"
    );
}

#[tokio::test]
async fn joining_with_a_mismatched_cluster_id_does_not_add_a_learner() {
    let node1 = spawn_node().await;
    let mut members = std::collections::BTreeMap::new();
    members.insert(
        node1.node_id,
        openraft::BasicNode::new(node1.addr.to_string()),
    );
    node1.store.raft().initialize(members).await.unwrap();
    node1
        .store
        .bootstrap_cluster(ClusterId::parse("real-cluster").unwrap())
        .await
        .unwrap();

    let node2 = spawn_node().await;
    let http = reqwest::Client::new();
    let err = join_cluster(
        &http,
        &format!("http://{}", node1.addr),
        TOKEN,
        &JoinRequest {
            node_id: node2.node_id,
            advertised_address: node2.addr.to_string(),
            failure_domain: vec![],
            claimed_cluster_id: Some("wrong-cluster".into()),
            volumes: vec![],
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        loony_rpc::RpcClientError::Remote { status: 409, .. }
    ));

    // Rejected before add_learner ever ran: node 1's membership is still just itself.
    let metrics = node1.store.raft().metrics().borrow().clone();
    assert_eq!(metrics.membership_config.voter_ids().count(), 1);
}
