//! Re-linking users after an identity provider reset, and admins making someone an Owner,
//! through the real Raft path.

use std::sync::Arc;

use loony_core::{OwnerId, ProjectId, UserId};
use time::OffsetDateTime;

use crate::billing::{LedgerReason, NewGiftCode};
use crate::commands::{CreateProject, Credential, CredentialKind, ProjectRole, User, UserSignIn};
use crate::error::MetaError;
use crate::raft::RaftMetadataStore;
use crate::raft::accounts::{relink_candidates, relink_refusal};
use crate::store::MetadataStore;

const ISSUER: &str = "https://auth.example";

async fn store() -> Arc<RaftMetadataStore> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("meta.redb");
    Arc::new(
        RaftMetadataStore::open_single_node(loony_core::NodeId::new(), "self:9100".into(), &path)
            .await
            .unwrap(),
    )
}

async fn sign_in(store: &RaftMetadataStore, subject: &str, email: &str, verified: bool) -> User {
    store
        .record_user_sign_in(UserSignIn {
            issuer: ISSUER.into(),
            subject: subject.into(),
            email: Some(email.into()),
            email_verified: verified,
            name: Some(format!("Name {subject}")),
        })
        .await
        .unwrap()
}

async fn project(store: &RaftMetadataStore, owner: UserId) -> ProjectId {
    store
        .create_project(CreateProject {
            name: "P".into(),
            created_by: owner,
            max_created: 100,
        })
        .await
        .unwrap()
        .project_id
}

async fn credit(store: &RaftMetadataStore, user_id: UserId, amount: u64) {
    let code = format!("CODE{}", ProjectId::new());
    store
        .create_gift_code(NewGiftCode {
            code_hash: code.clone(),
            hint: "TEST".into(),
            amount_paise: amount,
            expires_at: None,
            max_uses: 1,
            note: None,
            created_by: "test".into(),
        })
        .await
        .unwrap();
    store.redeem_gift_code(code, user_id).await.unwrap();
}

fn key(
    access_key: &str,
    kind: CredentialKind,
    user_id: UserId,
    project_id: Option<ProjectId>,
) -> Credential {
    Credential {
        access_key: access_key.into(),
        secret_key: "secret".into(),
        owner_id: OwnerId::new(),
        enabled: true,
        admin: false,
        kind,
        user_id: Some(user_id),
        expires_at: None,
        project_id,
        name: None,
        session_of: None,
        last_used_at: None,
        created_at: OffsetDateTime::now_utc(),
    }
}

/// The person had an account before the reset, then signed in again afterwards and got a
/// second, empty one.
async fn reset_scenario(store: &RaftMetadataStore) -> (User, User) {
    let old = sign_in(store, "old-sub", "Person@Example.com", true).await;
    let new = sign_in(store, "new-sub", "person@example.com", true).await;
    (old, new)
}

#[tokio::test]
async fn relinking_gives_the_old_user_the_new_identity_and_everything_the_new_one_did() {
    let store = store().await;
    let (old, new) = reset_scenario(&store).await;
    let old_project = project(&store, old.user_id).await;
    let new_project = project(&store, new.user_id).await;
    credit(&store, old.user_id, 500).await;
    credit(&store, new.user_id, 300).await;
    store
        .put_credential(key(
            "SESSION",
            CredentialKind::UserSession,
            new.user_id,
            None,
        ))
        .await
        .unwrap();
    store
        .put_credential(key(
            "APPKEY",
            CredentialKind::ProjectAccessKey,
            new.user_id,
            Some(new_project),
        ))
        .await
        .unwrap();

    let merged = store
        .relink_user(new.user_id, old.user_id, "admin:root".into())
        .await
        .unwrap();
    assert_eq!(merged.user_id, old.user_id);
    assert_eq!(merged.subject, "new-sub");
    assert_eq!(merged.created_at, old.created_at);

    // Signing in with the new identity now finds the old user, and the duplicate is gone.
    let again = sign_in(&store, "new-sub", "person@example.com", true).await;
    assert_eq!(again.user_id, old.user_id);
    assert!(store.get_user(new.user_id).await.unwrap().is_none());
    assert_eq!(store.list_users().await.unwrap().len(), 1);
    // The old identity can't sign in as this user any more.
    let stale = sign_in(&store, "old-sub", "someone@example.com", false).await;
    assert_ne!(stale.user_id, old.user_id);

    let mut projects: Vec<_> = store
        .list_user_memberships(old.user_id)
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.project_id, m.role))
        .collect();
    projects.sort_by_key(|(id, _)| *id);
    let mut expected = vec![
        (old_project, ProjectRole::Owner),
        (new_project, ProjectRole::Owner),
    ];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(projects, expected);
    assert!(
        store
            .list_user_memberships(new.user_id)
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(store.get_balance(old.user_id).await.unwrap(), 800);
    assert_eq!(store.get_balance(new.user_id).await.unwrap(), 0);
    let ledger = store.list_ledger(old.user_id, 10).await.unwrap();
    assert_eq!(ledger[0].amount_paise, 300);
    assert_eq!(ledger[0].balance_after_paise, 800);
    assert_eq!(
        ledger[0].reason,
        LedgerReason::AccountMerged {
            from_user: new.user_id
        }
    );
    assert_eq!(ledger[0].actor, "admin:root");

    // The duplicate's browser session ends; its app key keeps working, now under the old user.
    assert!(store.get_credential("SESSION").await.unwrap().is_none());
    let app_key = store.get_credential("APPKEY").await.unwrap().unwrap();
    assert_eq!(app_key.user_id, Some(old.user_id));
    assert!(app_key.enabled);

    // Projects the duplicate made count against the old user's limit.
    let created = store
        .create_project(CreateProject {
            name: "Q".into(),
            created_by: old.user_id,
            max_created: 2,
        })
        .await;
    assert!(
        matches!(created, Err(MetaError::ProjectLimitReached)),
        "{created:?}"
    );
}

#[tokio::test]
async fn a_membership_both_had_keeps_the_higher_role() {
    let store = store().await;
    let (old, new) = reset_scenario(&store).await;
    let owner = sign_in(&store, "owner", "owner@example.com", true).await;
    let shared = project(&store, owner.user_id).await;
    store.make_project_owner(shared, old.user_id).await.unwrap();
    store
        .set_member_role(shared, old.user_id, ProjectRole::Member)
        .await
        .unwrap();
    store.make_project_owner(shared, new.user_id).await.unwrap();

    store
        .relink_user(new.user_id, old.user_id, "admin:root".into())
        .await
        .unwrap();
    let member = store
        .get_member(shared, old.user_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(member.role, ProjectRole::Owner);
    assert!(
        store
            .get_member(shared, new.user_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.list_members(shared).await.unwrap().len(), 2);
}

#[tokio::test]
async fn relinking_is_refused_without_a_shared_verified_email() {
    let store = store().await;
    let old = sign_in(&store, "a", "a@example.com", true).await;
    let other = sign_in(&store, "b", "b@example.com", true).await;
    let unverified = sign_in(&store, "c", "a@example.com", false).await;

    for from in [other.user_id, unverified.user_id, old.user_id] {
        let result = store
            .relink_user(from, old.user_id, "admin:root".into())
            .await;
        assert!(
            matches!(result, Err(MetaError::CannotRelink(_))),
            "{result:?}"
        );
    }
    // Nothing changed.
    assert_eq!(store.list_users().await.unwrap().len(), 3);
    assert_eq!(
        sign_in(&store, "a", "a@example.com", true).await.user_id,
        old.user_id
    );
}

#[tokio::test]
async fn two_live_accounts_sharing_an_address_are_not_merged() {
    let store = store().await;
    let (old, new) = reset_scenario(&store).await;
    // The old identity signed in again after the new one appeared: both are in use.
    sign_in(&store, "old-sub", "person@example.com", true).await;
    let result = store
        .relink_user(new.user_id, old.user_id, "admin:root".into())
        .await;
    assert!(
        matches!(result, Err(MetaError::CannotRelink(_))),
        "{result:?}"
    );
    assert!(relink_candidates(&store.list_users().await.unwrap()).is_empty());
}

#[tokio::test]
async fn candidates_are_only_unambiguous_pairs() {
    let store = store().await;
    let (old, new) = reset_scenario(&store).await;
    // Three users with one address is for a person to sort out.
    sign_in(&store, "x1", "crowd@example.com", true).await;
    sign_in(&store, "x2", "crowd@example.com", true).await;
    sign_in(&store, "x3", "crowd@example.com", true).await;
    // One user alone has nothing to pair with.
    sign_in(&store, "solo", "solo@example.com", true).await;

    let pairs = relink_candidates(&store.list_users().await.unwrap());
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0.user_id, new.user_id);
    assert_eq!(pairs[0].1.user_id, old.user_id);
    assert_eq!(relink_refusal(&pairs[0].0, &pairs[0].1), None);
}

#[tokio::test]
async fn an_admin_can_make_anyone_an_owner() {
    let store = store().await;
    let gone = sign_in(&store, "gone", "gone@example.com", true).await;
    let helper = sign_in(&store, "helper", "helper@example.com", true).await;
    let member = sign_in(&store, "member", "member@example.com", true).await;
    let id = project(&store, gone.user_id).await;
    store.make_project_owner(id, member.user_id).await.unwrap();
    store
        .set_member_role(id, member.user_id, ProjectRole::Member)
        .await
        .unwrap();

    let added = store.make_project_owner(id, helper.user_id).await.unwrap();
    assert_eq!(added.role, ProjectRole::Owner);
    let promoted = store.make_project_owner(id, member.user_id).await.unwrap();
    assert_eq!(promoted.role, ProjectRole::Owner);
    assert_eq!(store.list_members(id).await.unwrap().len(), 3);

    let missing = store.make_project_owner(id, UserId::new()).await;
    assert!(
        matches!(missing, Err(MetaError::NoSuchUser(_))),
        "{missing:?}"
    );
    let no_project = store
        .make_project_owner(ProjectId::new(), helper.user_id)
        .await;
    assert!(
        matches!(no_project, Err(MetaError::NoSuchProject(_))),
        "{no_project:?}"
    );
}
