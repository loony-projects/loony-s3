//! Billing and storage limits through the real Raft path: money, limits and states have to
//! come out the same however requests interleave.

use std::sync::Arc;

use loony_core::{
    BucketId, BucketName, DurabilityPolicy, ETag, ObjectId, ObjectKey, ObjectManifest, OwnerId,
    PartManifest, ProjectId, Stripe, UserId, VersionId,
};
use time::{Duration, OffsetDateTime};

use crate::billing::{
    BillingPeriod, BillingTimings, CheckoutStatus, LedgerReason, NewCheckout, NewGiftCode, Plan,
    Step, SubscriptionState,
};
use crate::commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, CreateInvite, CreateProject, ProjectRole,
    UserSignIn,
};
use crate::error::MetaError;
use crate::raft::RaftMetadataStore;
use crate::store::MetadataStore;

const PRICE: u64 = 100_000;
const LIMIT: u64 = 1_000;

async fn store() -> Arc<RaftMetadataStore> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.keep().join("meta.redb");
    Arc::new(
        RaftMetadataStore::open_single_node(loony_core::NodeId::new(), "self:9100".into(), &path)
            .await
            .unwrap(),
    )
}

fn plan(period: BillingPeriod) -> Plan {
    Plan {
        id: "standard".into(),
        name: "Standard".into(),
        price_paise: PRICE,
        storage_bytes: LIMIT,
        period,
    }
}

async fn user(store: &RaftMetadataStore, email: &str) -> UserId {
    store
        .record_user_sign_in(UserSignIn {
            issuer: "https://issuer.example".into(),
            subject: email.into(),
            email: Some(email.into()),
            email_verified: true,
            name: None,
        })
        .await
        .unwrap()
        .user_id
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

struct Project {
    id: ProjectId,
    owner: OwnerId,
    bucket: BucketId,
}

async fn project(store: &RaftMetadataStore, owner: UserId) -> Project {
    let project = store
        .create_project(CreateProject {
            name: "P".into(),
            created_by: owner,
            max_created: 100,
        })
        .await
        .unwrap();
    Project {
        id: project.project_id,
        owner: project.owner_id,
        bucket: BucketId::new(),
    }
}

async fn paid_project(store: &RaftMetadataStore, owner: UserId) -> Project {
    let mut p = project(store, owner).await;
    credit(store, owner, PRICE).await;
    store
        .pay_for_project(p.id, owner, plan(BillingPeriod::Months(1)))
        .await
        .unwrap();
    p.bucket = store
        .create_bucket(CreateBucket {
            name: BucketName::parse(format!("b-{}", p.id.as_uuid().simple())).unwrap(),
            owner_id: p.owner,
            region: "us-east-1".into(),
        })
        .await
        .unwrap()
        .bucket_id;
    p
}

fn part(number: u32, size: u64) -> PartManifest {
    PartManifest {
        part_number: number,
        offset: 0,
        size,
        etag_md5: [number as u8; 16],
        stripes: vec![Stripe {
            stripe_index: 0,
            stripe_offset: 0,
            stripe_len: size,
            durability: DurabilityPolicy::Replicated { n: 1 },
            shards: vec![],
        }],
    }
}

fn object(bucket: BucketId, key: &str, size: u64) -> ObjectManifest {
    ObjectManifest {
        object_id: ObjectId::new(),
        bucket_id: bucket,
        key: ObjectKey::parse(key).unwrap(),
        version_id: VersionId::new(),
        size,
        etag: ETag::from_md5([1; 16]),
        sha256: [2; 32],
        content_type: "application/octet-stream".into(),
        user_metadata: Default::default(),
        created_at: OffsetDateTime::now_utc(),
        delete_marker: false,
        parts: vec![part(1, size)],
    }
}

#[tokio::test]
async fn paying_needs_enough_credit_and_records_it() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = project(&store, alice).await;

    let err = store
        .pay_for_project(p.id, alice, plan(BillingPeriod::Months(1)))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        MetaError::InsufficientCredit {
            balance: 0,
            needed: PRICE
        }
    ));
    assert!(store.get_subscription(p.id).await.unwrap().is_none());

    credit(&store, alice, PRICE + 5).await;
    let (sub, entry) = store
        .pay_for_project(p.id, alice, plan(BillingPeriod::Months(1)))
        .await
        .unwrap();
    assert_eq!(sub.state, SubscriptionState::Active);
    assert_eq!(sub.payer, alice);
    assert_eq!(entry.amount_paise, -(PRICE as i64));
    assert_eq!(entry.balance_after_paise, 5);
    assert_eq!(store.get_balance(alice).await.unwrap(), 5);

    let ledger = store.list_ledger(alice, 10).await.unwrap();
    assert_eq!(ledger.len(), 2, "the credit and the payment");
    assert!(
        matches!(
            ledger[0].reason,
            LedgerReason::Subscription {
                automatic: false,
                ..
            }
        ),
        "newest first"
    );
    assert!(matches!(ledger[1].reason, LedgerReason::GiftCode { .. }));
    let total: i64 = ledger.iter().map(|e| e.amount_paise).sum();
    assert_eq!(total, 5, "the ledger adds up to the balance");

    let again = store
        .pay_for_project(p.id, alice, plan(BillingPeriod::Months(1)))
        .await
        .unwrap_err();
    assert!(matches!(again, MetaError::AlreadyPaid));
}

#[tokio::test]
async fn only_an_owner_can_pay_or_cancel() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let bob = user(&store, "bob@example.com").await;
    let carol = user(&store, "carol@example.com").await;
    let p = paid_project(&store, alice).await;
    let invite = store
        .create_invite(CreateInvite {
            project_id: p.id,
            email: "bob@example.com".into(),
            role: ProjectRole::Member,
            invited_by: alice,
            expires_at: OffsetDateTime::now_utc() + Duration::hours(1),
        })
        .await
        .unwrap();
    store.accept_invite(invite.invite_id, bob).await.unwrap();

    credit(&store, bob, PRICE).await;
    let other = project(&store, alice).await;
    assert!(matches!(
        store
            .pay_for_project(other.id, bob, plan(BillingPeriod::Months(1)))
            .await
            .unwrap_err(),
        MetaError::NoSuchMember
    ));
    assert!(matches!(
        store
            .set_subscription_cancelled(p.id, bob, true)
            .await
            .unwrap_err(),
        MetaError::NotProjectOwner
    ));
    assert!(matches!(
        store
            .set_subscription_cancelled(p.id, carol, true)
            .await
            .unwrap_err(),
        MetaError::NoSuchMember
    ));
    assert_eq!(
        store.get_balance(bob).await.unwrap(),
        PRICE,
        "nothing was charged"
    );
}

#[tokio::test]
async fn running_a_step_twice_charges_once() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;
    credit(&store, alice, PRICE * 5).await;
    let sub = store.get_subscription(p.id).await.unwrap().unwrap();
    let due = sub.period_end;

    let first = store
        .advance_subscription(&sub, BillingTimings::default(), due)
        .await
        .unwrap();
    assert!(matches!(first, Some((_, Step::Renew { .. }))));
    // The same view of the subscription again, as from a second node or a retried tick.
    let second = store
        .advance_subscription(&sub, BillingTimings::default(), due)
        .await
        .unwrap();
    assert_eq!(second, None);
    assert_eq!(store.get_balance(alice).await.unwrap(), PRICE * 4);

    let renewed = store.get_subscription(p.id).await.unwrap().unwrap();
    assert_eq!(renewed.period_start, due);
    assert!(renewed.period_end > due);
    let ledger = store.list_ledger(alice, 1).await.unwrap();
    assert!(matches!(
        ledger[0].reason,
        LedgerReason::Subscription {
            automatic: true,
            ..
        }
    ));
    assert_eq!(ledger[0].actor, "system");
}

#[tokio::test]
async fn a_lapse_goes_through_each_state_and_paying_restores_it() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;
    let t = BillingTimings {
        grace_secs: 100,
        suspend_after_secs: 1000,
    };
    let step = |expected: SubscriptionState, at| {
        let store = store.clone();
        async move {
            let sub = store.get_subscription(p.id).await.unwrap().unwrap();
            assert_eq!(sub.state, expected);
            store.advance_subscription(&sub, t, at).await.unwrap()
        }
    };
    let end = store
        .get_subscription(p.id)
        .await
        .unwrap()
        .unwrap()
        .period_end;

    assert!(matches!(
        step(SubscriptionState::Active, end).await,
        Some((_, Step::EnterGrace { .. }))
    ));
    store
        .commit_manifest(object(p.bucket, "during-grace", 10))
        .await
        .unwrap();

    assert!(matches!(
        step(SubscriptionState::Grace, end + Duration::seconds(100)).await,
        Some((_, Step::EnterReadOnly { .. }))
    ));
    let refused = store
        .commit_manifest(object(p.bucket, "while-read-only", 10))
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        MetaError::ProjectNotWritable(SubscriptionState::ReadOnly)
    ));
    let refused = store
        .tombstone_object(p.bucket, &ObjectKey::parse("during-grace").unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(refused, MetaError::ProjectNotWritable(_)),
        "read-only means no deletes either"
    );

    assert!(matches!(
        step(SubscriptionState::ReadOnly, end + Duration::seconds(1100)).await,
        Some((_, Step::Suspend { .. }))
    ));

    credit(&store, alice, PRICE).await;
    let (sub, _) = store
        .pay_for_project(p.id, alice, plan(BillingPeriod::Months(1)))
        .await
        .unwrap();
    assert_eq!(sub.state, SubscriptionState::Active);
    store
        .commit_manifest(object(p.bucket, "after-paying", 10))
        .await
        .unwrap();
    assert_eq!(
        store.get_usage(p.owner).await.unwrap().objects,
        2,
        "nothing was deleted along the way"
    );
}

#[tokio::test]
async fn cancelling_runs_out_the_period_then_reads_only() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;
    credit(&store, alice, PRICE * 3).await;

    let sub = store
        .set_subscription_cancelled(p.id, alice, true)
        .await
        .unwrap();
    assert!(sub.cancel_at_period_end);
    store
        .commit_manifest(object(p.bucket, "still-paid", 10))
        .await
        .unwrap();

    let next = store
        .advance_subscription(&sub, BillingTimings::default(), sub.period_end)
        .await
        .unwrap();
    assert!(
        matches!(next, Some((ref s, Step::EnterReadOnly { .. })) if s.state == SubscriptionState::ReadOnly)
    );
    assert_eq!(
        store.get_balance(alice).await.unwrap(),
        PRICE * 3,
        "never charged again"
    );

    // Resuming before the end undoes the cancel.
    let q = paid_project(&store, alice).await;
    store
        .set_subscription_cancelled(q.id, alice, true)
        .await
        .unwrap();
    let resumed = store
        .set_subscription_cancelled(q.id, alice, false)
        .await
        .unwrap();
    assert!(!resumed.cancel_at_period_end);
}

#[tokio::test]
async fn an_unpaid_project_takes_on_nothing() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = project(&store, alice).await;
    let err = store
        .create_bucket(CreateBucket {
            name: BucketName::parse("unpaid").unwrap(),
            owner_id: p.owner,
            region: "us-east-1".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        MetaError::ProjectNotWritable(SubscriptionState::Unpaid)
    ));

    // Owners outside projects, like the root key's, have no limits at all.
    let free = OwnerId::new();
    let bucket = store
        .create_bucket(CreateBucket {
            name: BucketName::parse("free").unwrap(),
            owner_id: free,
            region: "us-east-1".into(),
        })
        .await
        .unwrap();
    store
        .commit_manifest(object(bucket.bucket_id, "big", u64::MAX / 4))
        .await
        .unwrap();
}

#[tokio::test]
async fn usage_follows_writes_overwrites_and_deletes() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;

    store
        .commit_manifest(object(p.bucket, "a", 300))
        .await
        .unwrap();
    store
        .commit_manifest(object(p.bucket, "b", 200))
        .await
        .unwrap();
    store
        .commit_manifest(object(p.bucket, "a", 100))
        .await
        .unwrap();
    let usage = store.get_usage(p.owner).await.unwrap();
    assert_eq!(
        (usage.bytes, usage.objects),
        (300, 2),
        "replacing counts only the difference"
    );

    store
        .tombstone_object(p.bucket, &ObjectKey::parse("b").unwrap())
        .await
        .unwrap();
    store
        .tombstone_object(p.bucket, &ObjectKey::parse("missing").unwrap())
        .await
        .unwrap();
    let usage = store.get_usage(p.owner).await.unwrap();
    assert_eq!((usage.bytes, usage.objects), (100, 1));
}

#[tokio::test]
async fn going_over_the_limit_is_refused_but_shrinking_isnt() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;

    store
        .commit_manifest(object(p.bucket, "a", LIMIT - 100))
        .await
        .unwrap();
    let err = store
        .commit_manifest(object(p.bucket, "b", 101))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        MetaError::QuotaExceeded {
            limit: LIMIT,
            used: 900,
            requested: 101
        }
    ));
    assert!(
        store
            .get_manifest(p.bucket, &ObjectKey::parse("b").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    store
        .commit_manifest(object(p.bucket, "b", 100))
        .await
        .unwrap();
    assert_eq!(
        store.get_usage(p.owner).await.unwrap().bytes,
        LIMIT,
        "exactly full is fine"
    );

    // Replacing a file with a smaller one is fine even when full.
    store
        .commit_manifest(object(p.bucket, "a", 10))
        .await
        .unwrap();
    assert!(matches!(store.check_write(p.owner, 890).await, Ok(())));
    assert!(matches!(
        store.check_write(p.owner, 891).await,
        Err(MetaError::QuotaExceeded { .. })
    ));
}

#[tokio::test]
async fn multipart_parts_count_while_in_progress() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;
    let begin = |key: &str| {
        let store = store.clone();
        let key = ObjectKey::parse(key).unwrap();
        let bucket = p.bucket;
        async move {
            store
                .begin_multipart(BeginMultipart {
                    bucket_id: bucket,
                    key,
                    content_type: "x".into(),
                    user_metadata: Default::default(),
                })
                .await
                .unwrap()
        }
    };

    let one = begin("one").await;
    let two = begin("two").await;
    store.record_part(one, part(1, 400)).await.unwrap();
    store.record_part(two, part(1, 400)).await.unwrap();
    let err = store.record_part(two, part(2, 201)).await.unwrap_err();
    assert!(
        matches!(err, MetaError::QuotaExceeded { .. }),
        "two uploads can't start within the limit and end over it"
    );
    store.record_part(one, part(1, 300)).await.unwrap();
    assert_eq!(
        store.get_usage(p.owner).await.unwrap().pending_bytes,
        700,
        "sending a part again counts the difference"
    );

    store.record_part(one, part(2, 50)).await.unwrap();
    let manifest = store
        .complete_multipart(CompleteMultipart {
            upload_id: one,
            requested_parts: vec![(1, hex([1; 16])), (2, hex([2; 16]))],
            content_type: "x".into(),
        })
        .await
        .unwrap();
    assert_eq!(manifest.size, 350);
    let usage = store.get_usage(p.owner).await.unwrap();
    assert_eq!(
        (usage.bytes, usage.objects, usage.pending_bytes),
        (350, 1, 400)
    );

    store.abort_multipart(two).await.unwrap();
    let usage = store.get_usage(p.owner).await.unwrap();
    assert_eq!((usage.bytes, usage.pending_bytes), (350, 0));
}

fn hex(bytes: [u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Uploads finishing at the same moment can't together go over the limit.
#[tokio::test]
async fn concurrent_writes_never_overshoot() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;

    let mut tasks = Vec::new();
    for i in 0..40 {
        let store = store.clone();
        let bucket = p.bucket;
        tasks.push(tokio::spawn(async move {
            store
                .commit_manifest(object(bucket, &format!("f{i}"), 30))
                .await
        }));
    }
    let mut stored = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => stored += 1,
            Err(MetaError::QuotaExceeded { .. }) => {}
            Err(other) => panic!("{other}"),
        }
    }
    assert_eq!(stored, LIMIT / 30, "as many as fit, and no more");
    let usage = store.get_usage(p.owner).await.unwrap();
    assert!(usage.bytes <= LIMIT);
    assert_eq!(usage.objects, stored);
}

#[tokio::test]
async fn gift_codes_follow_their_rules() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let bob = user(&store, "bob@example.com").await;
    let carol = user(&store, "carol@example.com").await;
    let new = |hash: &str, max_uses, expires_at| NewGiftCode {
        code_hash: hash.into(),
        hint: "ABCD".into(),
        amount_paise: 2_000_000,
        expires_at,
        max_uses,
        note: None,
        created_by: "admin".into(),
    };

    let shared = store
        .create_gift_code(new("SHARED", 2, None))
        .await
        .unwrap();
    assert!(matches!(
        store
            .create_gift_code(new("SHARED", 1, None))
            .await
            .unwrap_err(),
        MetaError::GiftCodeExists
    ));
    store
        .redeem_gift_code("SHARED".into(), alice)
        .await
        .unwrap();
    assert!(matches!(
        store
            .redeem_gift_code("SHARED".into(), alice)
            .await
            .unwrap_err(),
        MetaError::GiftCodeAlreadyRedeemed
    ));
    store.redeem_gift_code("SHARED".into(), bob).await.unwrap();
    assert!(matches!(
        store
            .redeem_gift_code("SHARED".into(), carol)
            .await
            .unwrap_err(),
        MetaError::GiftCodeUsedUp
    ));
    assert_eq!(store.get_balance(alice).await.unwrap(), 2_000_000);
    assert_eq!(store.get_balance(carol).await.unwrap(), 0);
    let redemptions = store.list_redemptions(shared.code_id).await.unwrap();
    assert_eq!(redemptions.len(), 2);

    let past = OffsetDateTime::now_utc() - Duration::seconds(1);
    store
        .create_gift_code(new("OLD", 1, Some(past)))
        .await
        .unwrap();
    assert!(matches!(
        store
            .redeem_gift_code("OLD".into(), carol)
            .await
            .unwrap_err(),
        MetaError::GiftCodeExpired
    ));

    let off = store.create_gift_code(new("OFF", 1, None)).await.unwrap();
    store
        .set_gift_code_disabled(off.code_id, true)
        .await
        .unwrap();
    assert!(matches!(
        store
            .redeem_gift_code("OFF".into(), carol)
            .await
            .unwrap_err(),
        MetaError::NoSuchGiftCode
    ));
    assert!(matches!(
        store
            .redeem_gift_code("NOPE".into(), carol)
            .await
            .unwrap_err(),
        MetaError::NoSuchGiftCode
    ));
    assert_eq!(store.get_balance(carol).await.unwrap(), 0);
    assert!(store.list_ledger(carol, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_single_use_code_redeemed_at_once_pays_out_once() {
    let store = store().await;
    store
        .create_gift_code(NewGiftCode {
            code_hash: "ONCE".into(),
            hint: "ONCE".into(),
            amount_paise: 100,
            expires_at: None,
            max_uses: 1,
            note: None,
            created_by: "admin".into(),
        })
        .await
        .unwrap();
    let mut users = Vec::new();
    for i in 0..10 {
        users.push(user(&store, &format!("u{i}@example.com")).await);
    }
    let tasks: Vec<_> = users
        .iter()
        .map(|&u| {
            let store = store.clone();
            tokio::spawn(async move { store.redeem_gift_code("ONCE".into(), u).await.is_ok() })
        })
        .collect();
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
    let mut total = 0;
    for u in users {
        total += store.get_balance(u).await.unwrap();
    }
    assert_eq!(total, 100);
}

#[tokio::test]
async fn a_checkout_pays_out_once_however_often_it_is_confirmed() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let new = || NewCheckout {
        user_id: alice,
        amount_paise: 500,
        project_id: None,
        plan_id: None,
        provider: "test".into(),
        provider_ref: None,
    };
    let checkout = store.create_checkout(new()).await.unwrap();
    assert_eq!(checkout.status, CheckoutStatus::Pending);
    let done = store
        .complete_checkout(checkout.checkout_id, true, Some("ref-1".into()))
        .await
        .unwrap();
    assert_eq!(done.status, CheckoutStatus::Succeeded);
    let again = store
        .complete_checkout(checkout.checkout_id, false, None)
        .await
        .unwrap();
    assert_eq!(
        again.status,
        CheckoutStatus::Succeeded,
        "the first outcome stands"
    );
    assert_eq!(store.get_balance(alice).await.unwrap(), 500);

    let failed = store.create_checkout(new()).await.unwrap();
    store
        .complete_checkout(failed.checkout_id, false, None)
        .await
        .unwrap();
    assert_eq!(store.get_balance(alice).await.unwrap(), 500);
    assert!(matches!(
        store
            .complete_checkout(loony_core::CheckoutId::new(), true, None)
            .await
            .unwrap_err(),
        MetaError::NoSuchCheckout
    ));
}

#[tokio::test]
async fn keys_follow_the_subscription() {
    use crate::commands::{Credential, CredentialKind};
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = project(&store, alice).await;
    let key = |kind| Credential {
        access_key: format!("K{}", ProjectId::new().as_uuid().simple()),
        secret_key: "s".into(),
        owner_id: p.owner,
        enabled: true,
        admin: false,
        kind,
        user_id: Some(alice),
        expires_at: None,
        project_id: Some(p.id),
        name: None,
        session_of: None,
        last_used_at: None,
        created_at: OffsetDateTime::now_utc(),
    };
    store
        .put_project_credential(key(CredentialKind::ProjectSession), 10)
        .await
        .unwrap();
    assert!(matches!(
        store
            .put_project_credential(key(CredentialKind::ProjectAccessKey), 10)
            .await
            .unwrap_err(),
        MetaError::ProjectNotWritable(SubscriptionState::Unpaid)
    ));
    credit(&store, alice, PRICE).await;
    store
        .pay_for_project(p.id, alice, plan(BillingPeriod::Months(1)))
        .await
        .unwrap();
    store
        .put_project_credential(key(CredentialKind::ProjectAccessKey), 10)
        .await
        .unwrap();
}

/// Upgrading builds the counters from what's already stored, and they match what keeping
/// them up to date would have given.
#[tokio::test]
async fn rebuilt_counters_match_the_kept_ones() {
    let store = store().await;
    let alice = user(&store, "alice@example.com").await;
    let p = paid_project(&store, alice).await;
    store
        .commit_manifest(object(p.bucket, "a", 120))
        .await
        .unwrap();
    store
        .commit_manifest(object(p.bucket, "b", 80))
        .await
        .unwrap();
    let upload = store
        .begin_multipart(BeginMultipart {
            bucket_id: p.bucket,
            key: ObjectKey::parse("c").unwrap(),
            content_type: "x".into(),
            user_metadata: Default::default(),
        })
        .await
        .unwrap();
    store.record_part(upload, part(1, 33)).await.unwrap();
    let kept = store.get_usage(p.owner).await.unwrap();

    assert!(!store.derived_indexes_built().await.unwrap());
    store.rebuild_derived_indexes().await.unwrap();
    assert!(store.derived_indexes_built().await.unwrap());
    assert_eq!(store.get_usage(p.owner).await.unwrap(), kept);
}

#[test]
fn billing_records_survive_a_snapshot() {
    use crate::raft::applier;
    use crate::raft::billing_apply;
    use crate::raft::snapshot::{restore_blob, snapshot_blob};
    use crate::raft::types::{ResolvedNewGiftCode, ResolvedRedeem};

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
    let at = OffsetDateTime::now_utc();
    let alice = UserId::new();
    let code = billing_apply::create_gift_code(
        &source,
        ResolvedNewGiftCode {
            code: NewGiftCode {
                code_hash: "H".into(),
                hint: "H".into(),
                amount_paise: 700,
                expires_at: None,
                max_uses: 3,
                note: None,
                created_by: "admin".into(),
            },
            code_id: loony_core::GiftCodeId::new(),
            at,
        },
    )
    .unwrap();
    billing_apply::redeem(
        &source,
        ResolvedRedeem {
            code_hash: "H".into(),
            user_id: alice,
            at,
            entry_id: loony_core::LedgerEntryId::new(),
        },
    )
    .unwrap();

    let blob = snapshot_blob(&source).unwrap();
    let txn = target.begin_write().unwrap();
    restore_blob(&txn, &blob).unwrap();
    txn.commit().unwrap();

    assert_eq!(billing_apply::get_balance(&target, alice).unwrap(), 700);
    assert_eq!(
        billing_apply::list_ledger(&target, alice, 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(billing_apply::list_gift_codes(&target).unwrap()[0].uses, 1);
    assert_eq!(
        billing_apply::list_redemptions(&target, code.code_id)
            .unwrap()
            .len(),
        1
    );
    // And the restored node refuses the same person redeeming it again.
    let again = billing_apply::redeem(
        &target,
        ResolvedRedeem {
            code_hash: "H".into(),
            user_id: alice,
            at,
            entry_id: loony_core::LedgerEntryId::new(),
        },
    );
    assert!(matches!(
        again.unwrap_err(),
        MetaError::GiftCodeAlreadyRedeemed
    ));
}
