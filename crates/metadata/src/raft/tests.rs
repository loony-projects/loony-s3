//! Single-voter sanity tests for [`RaftMetadataStore`] (Phase 8): every mutation goes
//! through `Raft::client_write` and every read bypasses it (`store.rs`'s module doc), so
//! these exercise that whole path end-to-end even with only one voter. Multi-node
//! behavior (leader election, failover, partition, snapshot transfer) is covered by the
//! 3-node integration test in `crates/rpc/tests/raft_cluster.rs`, which needs the real
//! HTTP network transport this crate deliberately doesn't depend on.

use loony_core::{BucketName, ClusterId, NodeId, NodeState, OwnerId};

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

fn sign_in(subject: &str, email: &str) -> crate::commands::UserSignIn {
    crate::commands::UserSignIn {
        issuer: "https://issuer.example".into(),
        subject: subject.into(),
        email: Some(email.into()),
        email_verified: true,
        name: Some("A Person".into()),
    }
}

#[tokio::test]
async fn a_first_sign_in_creates_a_user_and_later_ones_refresh_it() {
    let (store, _node_id, _path) = store().await;

    let first = store.record_user_sign_in(sign_in("sub-1", "old@example.com")).await.unwrap();
    assert!(first.enabled);
    assert!(!first.can_create_projects, "creating projects needs an admin's approval");

    let second = store.record_user_sign_in(sign_in("sub-1", "new@example.com")).await.unwrap();
    assert_eq!(second.user_id, first.user_id);
    assert_eq!(second.email.as_deref(), Some("new@example.com"));
    assert_eq!(second.created_at, first.created_at);
    assert!(second.last_sign_in_at >= first.last_sign_in_at);
    assert_eq!(store.list_users().await.unwrap().len(), 1);

    let other = store.record_user_sign_in(sign_in("sub-2", "x@example.com")).await.unwrap();
    assert_ne!(other.user_id, first.user_id);
    assert_eq!(store.list_users().await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_same_subject_from_another_issuer_is_another_user() {
    let (store, _node_id, _path) = store().await;
    let first = store.record_user_sign_in(sign_in("sub-1", "a@example.com")).await.unwrap();
    let mut elsewhere = sign_in("sub-1", "a@example.com");
    elsewhere.issuer = "https://other-issuer.example".into();
    let second = store.record_user_sign_in(elsewhere).await.unwrap();
    assert_ne!(first.user_id, second.user_id);
}

#[tokio::test]
async fn signing_in_never_changes_what_an_admin_controls() {
    let (store, _node_id, _path) = store().await;
    let user = store.record_user_sign_in(sign_in("sub-1", "a@example.com")).await.unwrap();

    let access = crate::commands::UserAccess {
        enabled: false,
        can_create_projects: true,
    };
    store.set_user_access(user.user_id, access).await.unwrap();

    let again = store.record_user_sign_in(sign_in("sub-1", "a@example.com")).await.unwrap();
    assert!(!again.enabled);
    assert!(again.can_create_projects);

    let stored = store.get_user(user.user_id).await.unwrap().unwrap();
    assert_eq!(stored, again);
}

#[tokio::test]
async fn changing_an_unknown_users_access_is_a_business_error() {
    let (store, _node_id, _path) = store().await;
    let err = store
        .set_user_access(
            loony_core::UserId::new(),
            crate::commands::UserAccess {
                enabled: true,
                can_create_projects: false,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, crate::error::MetaError::NoSuchUser(_)));
}

/// A node that joins late gets its state from a snapshot, so users must be in it, and a
/// sign-in after restoring must still find the same user.
#[test]
fn users_survive_a_snapshot_and_restore() {
    use crate::raft::applier;
    use crate::raft::snapshot::{restore_blob, snapshot_blob};
    use crate::raft::types::ResolvedUserSignIn;

    let open = |path: &std::path::Path| {
        let db = redb::Database::create(path).unwrap();
        let txn = db.begin_write().unwrap();
        for table in applier::ALL_TABLES {
            txn.open_table(*table).unwrap();
        }
        txn.commit().unwrap();
        db
    };
    let dir = tempfile::tempdir().unwrap();
    let source = open(&dir.path().join("source.redb"));
    let target = open(&dir.path().join("target.redb"));

    let at = time::OffsetDateTime::now_utc();
    let user = applier::record_user_sign_in(
        &source,
        ResolvedUserSignIn {
            sign_in: sign_in("sub-1", "a@example.com"),
            user_id: loony_core::UserId::new(),
            at,
        },
    )
    .unwrap();

    let blob = snapshot_blob(&source).unwrap();
    let txn = target.begin_write().unwrap();
    restore_blob(&txn, &blob).unwrap();
    txn.commit().unwrap();

    assert_eq!(applier::get_user(&target, user.user_id).unwrap().unwrap(), user);

    let again = applier::record_user_sign_in(
        &target,
        ResolvedUserSignIn {
            sign_in: sign_in("sub-1", "a@example.com"),
            user_id: loony_core::UserId::new(),
            at,
        },
    )
    .unwrap();
    assert_eq!(again.user_id, user.user_id);
}

/// `apply()` runs separately on every node, so it must give the same result on each:
/// applying the same command to two databases has to leave them identical.
#[test]
fn applying_a_sign_in_is_deterministic() {
    use crate::raft::applier;
    use crate::raft::types::ResolvedUserSignIn;

    let dir = tempfile::tempdir().unwrap();
    let command = ResolvedUserSignIn {
        sign_in: sign_in("sub-1", "a@example.com"),
        user_id: loony_core::UserId::new(),
        at: time::OffsetDateTime::now_utc(),
    };
    let apply = |name: &str| {
        let db = redb::Database::create(dir.path().join(name)).unwrap();
        let txn = db.begin_write().unwrap();
        for table in applier::ALL_TABLES {
            txn.open_table(*table).unwrap();
        }
        txn.commit().unwrap();
        applier::record_user_sign_in(&db, command.clone()).unwrap()
    };
    assert_eq!(apply("a.redb"), apply("b.redb"));
}
