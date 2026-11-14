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
    assert!(matches!(
        err,
        crate::error::MetaError::BucketAlreadyExists(_)
    ));
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

    let first = store
        .record_user_sign_in(sign_in("sub-1", "old@example.com"))
        .await
        .unwrap();
    assert!(first.enabled);

    let second = store
        .record_user_sign_in(sign_in("sub-1", "new@example.com"))
        .await
        .unwrap();
    assert_eq!(second.user_id, first.user_id);
    assert_eq!(second.email.as_deref(), Some("new@example.com"));
    assert_eq!(second.created_at, first.created_at);
    assert!(second.last_sign_in_at >= first.last_sign_in_at);
    assert_eq!(store.list_users().await.unwrap().len(), 1);

    let other = store
        .record_user_sign_in(sign_in("sub-2", "x@example.com"))
        .await
        .unwrap();
    assert_ne!(other.user_id, first.user_id);
    assert_eq!(store.list_users().await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_same_subject_from_another_issuer_is_another_user() {
    let (store, _node_id, _path) = store().await;
    let first = store
        .record_user_sign_in(sign_in("sub-1", "a@example.com"))
        .await
        .unwrap();
    let mut elsewhere = sign_in("sub-1", "a@example.com");
    elsewhere.issuer = "https://other-issuer.example".into();
    let second = store.record_user_sign_in(elsewhere).await.unwrap();
    assert_ne!(first.user_id, second.user_id);
}

#[tokio::test]
async fn signing_in_never_changes_what_an_admin_controls() {
    let (store, _node_id, _path) = store().await;
    let user = store
        .record_user_sign_in(sign_in("sub-1", "a@example.com"))
        .await
        .unwrap();

    let access = crate::commands::UserAccess { enabled: false };
    store.set_user_access(user.user_id, access).await.unwrap();

    let again = store
        .record_user_sign_in(sign_in("sub-1", "a@example.com"))
        .await
        .unwrap();
    assert!(!again.enabled);

    let stored = store.get_user(user.user_id).await.unwrap().unwrap();
    assert_eq!(stored, again);
}

#[tokio::test]
async fn changing_an_unknown_users_access_is_a_business_error() {
    let (store, _node_id, _path) = store().await;
    let err = store
        .set_user_access(
            loony_core::UserId::new(),
            crate::commands::UserAccess { enabled: true },
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

    assert_eq!(
        applier::get_user(&target, user.user_id).unwrap().unwrap(),
        user
    );

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

async fn signed_in(store: &RaftMetadataStore, email: &str) -> loony_core::UserId {
    store
        .record_user_sign_in(sign_in(email, email))
        .await
        .unwrap()
        .user_id
}

/// Project rules through the real Raft path, rather than the direct calls the console's
/// tests go through underneath.
#[tokio::test]
async fn projects_go_through_raft_and_keep_their_rules() {
    use crate::commands::{CreateProject, ProjectRole};
    use crate::error::MetaError;

    let (store, _node_id, _path) = store().await;
    let alice = signed_in(&store, "alice@example.com").await;
    let project = store
        .create_project(CreateProject {
            name: "P".into(),
            created_by: alice,
            max_created: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get_project(project.project_id)
            .await
            .unwrap()
            .unwrap(),
        project
    );

    let over = store
        .create_project(CreateProject {
            name: "Q".into(),
            created_by: alice,
            max_created: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(over, MetaError::ProjectLimitReached));

    let member = store
        .get_member(project.project_id, alice)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member.role, ProjectRole::Owner);
    assert!(matches!(
        store
            .remove_member(project.project_id, alice)
            .await
            .unwrap_err(),
        MetaError::LastOwner
    ));

    let renamed = store
        .rename_project(project.project_id, "New".into())
        .await
        .unwrap();
    assert_eq!(renamed.name, "New");
    store.delete_project(project.project_id).await.unwrap();
    assert!(
        store
            .get_project(project.project_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.delete_project(project.project_id).await.unwrap_err(),
        MetaError::NoSuchProject(_)
    ));
}

/// Both Owners demoting each other at once must not leave a project with none.
#[tokio::test]
async fn concurrent_demotions_still_leave_an_owner() {
    use crate::commands::{CreateInvite, CreateProject, ProjectRole};

    let (store, _node_id, _path) = store().await;
    let store = std::sync::Arc::new(store);
    let alice = signed_in(&store, "alice@example.com").await;
    let bob = signed_in(&store, "bob@example.com").await;
    let project = store
        .create_project(CreateProject {
            name: "P".into(),
            created_by: alice,
            max_created: 1,
        })
        .await
        .unwrap();
    let invite = store
        .create_invite(CreateInvite {
            project_id: project.project_id,
            email: "bob@example.com".into(),
            role: ProjectRole::Owner,
            invited_by: alice,
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        })
        .await
        .unwrap();
    store.accept_invite(invite.invite_id, bob).await.unwrap();

    let (one, two) = tokio::join!(
        store.set_member_role(project.project_id, alice, ProjectRole::Member),
        store.set_member_role(project.project_id, bob, ProjectRole::Member),
    );
    assert_ne!(one.is_ok(), two.is_ok(), "exactly one may succeed");

    let owners = store
        .list_members(project.project_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role == ProjectRole::Owner)
        .count();
    assert_eq!(owners, 1);
}

/// A session key must not slip in just as its user is removed.
#[tokio::test]
async fn a_session_key_needs_a_current_member() {
    use crate::commands::{CreateProject, Credential, CredentialKind};
    use crate::error::MetaError;

    let (store, _node_id, _path) = store().await;
    let alice = signed_in(&store, "alice@example.com").await;
    let outsider = signed_in(&store, "outsider@example.com").await;
    let project = store
        .create_project(CreateProject {
            name: "P".into(),
            created_by: alice,
            max_created: 1,
        })
        .await
        .unwrap();

    let key = |user_id, owner_id| Credential {
        access_key: "KEY".into(),
        secret_key: "secret".into(),
        owner_id,
        enabled: true,
        admin: false,
        kind: CredentialKind::ProjectSession,
        user_id: Some(user_id),
        expires_at: None,
        project_id: Some(project.project_id),
        name: None,
        session_of: None,
        last_used_at: None,
        created_at: time::OffsetDateTime::now_utc(),
    };
    assert!(matches!(
        store
            .put_project_credential(key(outsider, project.owner_id), 10)
            .await
            .unwrap_err(),
        MetaError::NoSuchMember
    ));
    assert!(store.get_credential("KEY").await.unwrap().is_none());

    // Nor can a project key be pointed at some other owner's buckets.
    assert!(
        store
            .put_project_credential(key(alice, OwnerId::new()), 10)
            .await
            .is_err(),
        "the key's owner must be the project's"
    );

    store
        .put_project_credential(key(alice, project.owner_id), 10)
        .await
        .unwrap();
    store
        .set_project_enabled(project.project_id, false)
        .await
        .unwrap();
    assert!(
        store.get_credential("KEY").await.unwrap().is_none(),
        "disabling ends session keys"
    );
    assert!(matches!(
        store
            .put_project_credential(key(alice, project.owner_id), 10)
            .await
            .unwrap_err(),
        MetaError::ProjectDisabled(_)
    ));
}

#[tokio::test]
async fn an_owner_from_before_projects_can_become_one() {
    use crate::commands::{AdoptOwner, Credential, CredentialKind, ProjectRole};
    use crate::error::MetaError;

    let (store, _node_id, _path) = store().await;
    let alice = signed_in(&store, "alice@example.com").await;
    let key = |access_key: &str, owner_id, admin| Credential {
        access_key: access_key.into(),
        secret_key: "secret".into(),
        owner_id,
        enabled: true,
        admin,
        kind: CredentialKind::Standard,
        user_id: None,
        expires_at: None,
        project_id: None,
        name: None,
        session_of: None,
        last_used_at: None,
        created_at: time::OffsetDateTime::now_utc(),
    };
    let owner = OwnerId::new();
    store
        .put_credential(key("OLD", owner, false))
        .await
        .unwrap();
    let bucket = store
        .create_bucket(CreateBucket {
            name: BucketName::parse("old-bucket").unwrap(),
            owner_id: owner,
            region: "us-east-1".into(),
        })
        .await
        .unwrap();
    let root = OwnerId::new();
    store.put_credential(key("ROOT", root, true)).await.unwrap();

    let adopt = |owner_id, user_id| AdoptOwner {
        owner_id,
        name: "Imported".into(),
        user_id,
    };

    let project = store.adopt_owner(adopt(owner, alice)).await.unwrap();
    assert_eq!(project.owner_id, owner);
    assert_eq!(project.owner_id, bucket.owner_id);
    let moved = store.get_credential("OLD").await.unwrap().unwrap();
    assert_eq!(moved.kind, CredentialKind::ProjectAccessKey);
    assert_eq!(moved.project_id, Some(project.project_id));
    assert_eq!(moved.name.as_deref(), Some("Imported key"));
    assert_eq!(moved.secret_key, "secret", "the app using it keeps working");
    let member = store
        .get_member(project.project_id, alice)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member.role, ProjectRole::Owner);

    assert!(matches!(
        store.adopt_owner(adopt(owner, alice)).await.unwrap_err(),
        MetaError::OwnerAlreadyInProject(_)
    ));
    assert!(matches!(
        store.adopt_owner(adopt(root, alice)).await.unwrap_err(),
        MetaError::OwnerIsAdmin(_)
    ));
    assert!(
        store
            .get_credential("ROOT")
            .await
            .unwrap()
            .unwrap()
            .project_id
            .is_none()
    );
    assert!(matches!(
        store
            .adopt_owner(adopt(OwnerId::new(), alice))
            .await
            .unwrap_err(),
        MetaError::NoSuchOwner(_)
    ));
    let other = OwnerId::new();
    store
        .put_credential(key("OTHER", other, false))
        .await
        .unwrap();
    assert!(matches!(
        store
            .adopt_owner(adopt(other, loony_core::UserId::new()))
            .await
            .unwrap_err(),
        MetaError::NoSuchUser(_)
    ));
    assert!(
        store
            .get_credential("OTHER")
            .await
            .unwrap()
            .unwrap()
            .project_id
            .is_none()
    );
}

/// Late joiners get their state from a snapshot, so all of this must be in it.
#[tokio::test]
async fn projects_survive_a_snapshot_and_restore() {
    use crate::commands::{CreateInvite, CreateProject, ProjectRole};
    use crate::raft::applier;
    use crate::raft::projects as apply;
    use crate::raft::snapshot::{restore_blob, snapshot_blob};
    use crate::raft::types::{ResolvedCreateInvite, ResolvedCreateProject};

    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| {
        let db = redb::Database::create(dir.path().join(name)).unwrap();
        let txn = db.begin_write().unwrap();
        for table in applier::ALL_TABLES {
            txn.open_table(*table).unwrap();
        }
        txn.commit().unwrap();
        db
    };
    let source = open("source.redb");
    let target = open("target.redb");
    let at = time::OffsetDateTime::now_utc();
    let alice = loony_core::UserId::new();

    let project = apply::create_project(
        &source,
        ResolvedCreateProject {
            cmd: CreateProject {
                name: "P".into(),
                created_by: alice,
                max_created: 1,
            },
            project_id: loony_core::ProjectId::new(),
            owner_id: OwnerId::new(),
            at,
        },
    )
    .unwrap();
    let invite = apply::create_invite(
        &source,
        ResolvedCreateInvite {
            cmd: CreateInvite {
                project_id: project.project_id,
                email: "bob@example.com".into(),
                role: ProjectRole::Member,
                invited_by: alice,
                expires_at: at + time::Duration::days(7),
            },
            invite_id: loony_core::InviteId::new(),
            at,
        },
    )
    .unwrap();

    let blob = snapshot_blob(&source).unwrap();
    let txn = target.begin_write().unwrap();
    restore_blob(&txn, &blob).unwrap();
    txn.commit().unwrap();

    assert_eq!(
        apply::get_project(&target, project.project_id).unwrap(),
        Some(project.clone())
    );
    assert_eq!(
        apply::get_invite(&target, invite.invite_id).unwrap(),
        Some(invite)
    );
    assert_eq!(
        apply::list_members(&target, project.project_id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn last_used_only_ever_moves_forward() {
    use crate::commands::Credential;

    let (store, _node_id, _path) = store().await;
    store
        .put_credential(Credential {
            access_key: "KEY".into(),
            secret_key: "secret".into(),
            owner_id: OwnerId::new(),
            enabled: true,
            admin: false,
            kind: Default::default(),
            user_id: None,
            expires_at: None,
            project_id: None,
            name: None,
            session_of: None,
            last_used_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    store.touch_credential("KEY", now).await.unwrap();
    store
        .touch_credential("KEY", now - time::Duration::hours(1))
        .await
        .unwrap();
    let used = store
        .get_credential("KEY")
        .await
        .unwrap()
        .unwrap()
        .last_used_at
        .unwrap();
    assert_eq!(used.unix_timestamp(), now.unix_timestamp());

    // A key deleted in the meantime stays deleted.
    store.delete_credential("KEY").await.unwrap();
    store.touch_credential("KEY", now).await.unwrap();
    assert!(store.get_credential("KEY").await.unwrap().is_none());
}
