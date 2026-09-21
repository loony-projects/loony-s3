//! Single-voter sanity tests for [`RaftMetadataStore`] (Phase 8): every mutation goes
//! through `Raft::client_write` and every read bypasses it (`store.rs`'s module doc), so
//! these exercise that whole path end-to-end even with only one voter. Multi-node
//! behavior (leader election, failover, partition, snapshot transfer) is covered by the
//! 3-node integration test in `crates/rpc/tests/raft_cluster.rs`, which needs the real
//! HTTP network transport this crate deliberately doesn't depend on.

use s3_core::{BucketName, ClusterId, NodeId, NodeState, OwnerId};

use crate::commands::CreateBucket;
use crate::raft::RaftMetadataStore;
use crate::store::MetadataStore;

async fn store() -> (RaftMetadataStore, NodeId, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("meta.redb");
    let node_id = NodeId::new();
    let store = RaftMetadataStore::open_single_node(node_id, "self:9100".into(), &path)
        .await
        .unwrap();
    (store, node_id, path)
}

#[tokio::test]
async fn single_voter_group_elects_itself_leader_and_serves_writes() {
    let (store, node_id, _path) = store().await;

    // A fresh single-voter group starts uninitialized-then-self-initializes inside
    // `open_single_node`; give it a moment to run its election (there's no peer to
    // contend with, so this is fast, but it's still an async state transition).
    let metrics = store.raft().metrics();
    for _ in 0..200 {
        if metrics.borrow().current_leader == Some(node_id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(metrics.borrow().current_leader, Some(node_id));

    let bucket = store
        .create_bucket(CreateBucket {
            name: BucketName::parse("raft-bucket").unwrap(),
            owner_id: OwnerId::new(),
            region: "us-east-1".into(),
        })
        .await
        .unwrap();

    let fetched = store.get_bucket(&bucket.name).await.unwrap().unwrap();
    assert_eq!(fetched.bucket_id, bucket.bucket_id);
}

#[tokio::test]
async fn duplicate_bucket_name_is_a_business_error_not_a_raft_error() {
    let (store, _node_id, _path) = store().await;
    let cmd = CreateBucket {
        name: BucketName::parse("dup").unwrap(),
        owner_id: OwnerId::new(),
        region: "us-east-1".into(),
    };
    store.create_bucket(cmd.clone()).await.unwrap();
    let err = store.create_bucket(cmd).await.unwrap_err();
    assert!(matches!(err, crate::error::MetaError::BucketAlreadyExists(_)));
}

#[tokio::test]
async fn cluster_bootstrap_and_node_registry_round_trip_through_raft() {
    let (store, node_id, _path) = store().await;

    assert_eq!(store.get_cluster_id().await.unwrap(), None);
    let cluster_id = ClusterId::parse("c1").unwrap();
    store.bootstrap_cluster(cluster_id.clone()).await.unwrap();
    assert_eq!(store.get_cluster_id().await.unwrap(), Some(cluster_id));

    let info = store
        .register_node(crate::commands::RegisterNode {
            node_id,
            advertised_address: "self:9100".into(),
            failure_domain: vec![],
            volumes: vec![],
        })
        .await
        .unwrap();
    assert_eq!(info.generation, 0);

    store
        .update_node_state(node_id, NodeState::Healthy)
        .await
        .unwrap();
    let nodes = store.list_nodes().await.unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].state, NodeState::Healthy);
}

#[tokio::test]
async fn state_survives_a_restart() {
    let (store, node_id, path) = store().await;
    let bucket_id;
    {
        let bucket = store
            .create_bucket(CreateBucket {
                name: BucketName::parse("persisted").unwrap(),
                owner_id: OwnerId::new(),
                region: "us-east-1".into(),
            })
            .await
            .unwrap();
        bucket_id = bucket.bucket_id;
    }
    // `Raft::new` spawns background tasks that each hold a clone of the `Arc<Database>`
    // (log store, state machine) -- dropping `store` alone doesn't stop them, so the
    // redb file lock outlives it. `shutdown()` stops those tasks first.
    store.raft().shutdown().await.unwrap();
    drop(store);

    let reopened = RaftMetadataStore::open_single_node(node_id, "self:9100".into(), &path)
        .await
        .unwrap();
    let fetched = reopened
        .get_bucket(&BucketName::parse("persisted").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.bucket_id, bucket_id);
}
