//! `RedbMetadataStore`: the one production implementation of [`MetadataStore`].
//!
//! Every mutating method wraps its work in a single redb write transaction — redb
//! serializes writers, so that transaction boundary *is* the concurrency-control
//! mechanism (architecture.md §62: "do not use a global mutex; prefer metadata
//! transactions"), not an app-level lock on top of it.
//!
//! redb is synchronous; every call here runs inside `tokio::task::spawn_blocking` so it
//! never blocks a Tokio worker thread (architecture.md §92).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loony_core::{
    Bucket, BucketId, BucketName, CheckoutId, ClusterId, GiftCodeId, InviteId, LedgerEntryId,
    NodeId, NodeInfo, NodeState, ObjectId, ObjectKey, ObjectManifest, OwnerId, PartManifest,
    ProjectId, UploadId, UserId, VersionId,
};
use redb::{Database, ReadableTable, TableDefinition};
use time::OffsetDateTime;

use crate::billing::{
    BillingTimings, Checkout, GiftCode, GiftCodeRedemption, LedgerEntry, NewCheckout, NewGiftCode,
    OwnerUsage, Plan, Step, Subscription,
};
use crate::commands::{
    AdoptOwner, BeginMultipart, CompleteMultipart, CreateBucket, CreateInvite, CreateProject,
    Credential, ListObjectsPage, ListObjectsQuery, MultipartUploadState, ObjectSummary,
    PartSummary, Project, ProjectInvite, ProjectMember, ProjectRole, RegisterNode, StorageStats,
    User, UserAccess, UserSignIn,
};
use crate::error::MetaError;
use crate::raft::applier;
use crate::raft::billing_apply;
use crate::raft::projects;
use crate::raft::types::{
    ResolvedAdoptOwner, ResolvedAdvance, ResolvedBeginMultipart, ResolvedCancel,
    ResolvedCompleteCheckout, ResolvedCompleteMultipart, ResolvedCreateBucket,
    ResolvedCreateInvite, ResolvedCreateProject, ResolvedInviteAnswer, ResolvedNewCheckout,
    ResolvedNewGiftCode, ResolvedPay, ResolvedRedeem, ResolvedTouchCredential, ResolvedUserSignIn,
};

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}
use crate::store::MetadataStore;

const BUCKETS: TableDefinition<&str, &[u8]> = TableDefinition::new("buckets");
const OBJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("objects");
const MULTIPART: TableDefinition<&str, &[u8]> = TableDefinition::new("multipart_uploads");
const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
const NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
const CLUSTER: TableDefinition<&str, &[u8]> = TableDefinition::new("cluster");
const CLUSTER_ID_KEY: &str = "cluster_id";

pub struct RedbMetadataStore {
    db: Arc<Database>,
}

fn db_err<E: std::fmt::Display>(e: E) -> MetaError {
    MetaError::Db(e.to_string())
}

fn object_row_prefix(bucket_id: BucketId) -> String {
    format!("{bucket_id}\0")
}

fn object_row_key(bucket_id: BucketId, key: &ObjectKey) -> String {
    format!("{bucket_id}\0{key}")
}

impl RedbMetadataStore {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, MetaError> {
        let path = path.into();
        let db = tokio::task::spawn_blocking(move || -> Result<Database, MetaError> {
            create_parent_dir(&path)?;
            let db = Database::create(&path).map_err(db_err)?;
            let write_txn = db.begin_write().map_err(db_err)?;
            write_txn.open_table(BUCKETS).map_err(db_err)?;
            write_txn.open_table(OBJECTS).map_err(db_err)?;
            write_txn.open_table(MULTIPART).map_err(db_err)?;
            write_txn.open_table(CREDENTIALS).map_err(db_err)?;
            write_txn.open_table(applier::USERS).map_err(db_err)?;
            write_txn
                .open_table(applier::USER_SUBJECTS)
                .map_err(db_err)?;
            write_txn.open_table(applier::PROJECTS).map_err(db_err)?;
            write_txn
                .open_table(applier::PROJECT_MEMBERS)
                .map_err(db_err)?;
            write_txn
                .open_table(applier::PROJECT_INVITES)
                .map_err(db_err)?;
            for table in [
                billing_apply::OWNER_USAGE,
                billing_apply::BUCKET_OWNERS,
                billing_apply::OWNER_PROJECTS,
                billing_apply::SUBSCRIPTIONS,
                billing_apply::CREDITS,
                billing_apply::LEDGER,
                billing_apply::GIFT_CODES,
                billing_apply::GIFT_CODE_HASHES,
                billing_apply::GIFT_REDEMPTIONS,
                billing_apply::CHECKOUTS,
            ] {
                write_txn.open_table(table).map_err(db_err)?;
            }
            write_txn.open_table(NODES).map_err(db_err)?;
            write_txn.open_table(CLUSTER).map_err(db_err)?;
            write_txn.commit().map_err(db_err)?;
            Ok(db)
        })
        .await
        .map_err(|e| MetaError::TaskPanicked(e.to_string()))??;

        Ok(Self { db: Arc::new(db) })
    }

    async fn blocking<F, T>(&self, f: F) -> Result<T, MetaError>
    where
        F: FnOnce(&Database) -> Result<T, MetaError> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| MetaError::TaskPanicked(e.to_string()))?
    }
}

fn create_parent_dir(path: &Path) -> Result<(), MetaError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(db_err)?;
    }
    Ok(())
}

#[async_trait]
impl MetadataStore for RedbMetadataStore {
    async fn create_bucket(&self, cmd: CreateBucket) -> Result<Bucket, MetaError> {
        let resolved = ResolvedCreateBucket {
            cmd,
            bucket_id: BucketId::new(),
            created_at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| applier::create_bucket(db, resolved))
            .await
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<(), MetaError> {
        let name = name.clone();
        self.blocking(move |db| applier::delete_bucket(db, &name))
            .await
    }

    async fn get_bucket(&self, name: &BucketName) -> Result<Option<Bucket>, MetaError> {
        let name = name.clone();
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(BUCKETS).map_err(db_err)?;
            match table.get(name.as_str()).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn list_all_buckets(&self) -> Result<Vec<Bucket>, MetaError> {
        self.blocking(applier::list_all_buckets).await
    }

    async fn storage_stats(&self) -> Result<StorageStats, MetaError> {
        self.blocking(applier::storage_stats).await
    }

    async fn list_credentials(&self) -> Result<Vec<Credential>, MetaError> {
        self.blocking(applier::list_credentials).await
    }

    async fn delete_credential(&self, access_key: &str) -> Result<(), MetaError> {
        let access_key = access_key.to_string();
        self.blocking(move |db| applier::delete_credential(db, &access_key))
            .await
    }

    async fn record_user_sign_in(&self, sign_in: UserSignIn) -> Result<User, MetaError> {
        let resolved = ResolvedUserSignIn {
            sign_in,
            user_id: UserId::new(),
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| applier::record_user_sign_in(db, resolved))
            .await
    }

    async fn create_project(&self, cmd: CreateProject) -> Result<Project, MetaError> {
        let resolved = ResolvedCreateProject {
            cmd,
            project_id: ProjectId::new(),
            owner_id: OwnerId::new(),
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| projects::create_project(db, resolved))
            .await
    }

    async fn adopt_owner(&self, cmd: AdoptOwner) -> Result<Project, MetaError> {
        let resolved = ResolvedAdoptOwner {
            cmd,
            project_id: ProjectId::new(),
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| projects::adopt_owner(db, resolved))
            .await
    }

    async fn get_project(&self, project_id: ProjectId) -> Result<Option<Project>, MetaError> {
        self.blocking(move |db| projects::get_project(db, project_id))
            .await
    }

    async fn list_projects(&self) -> Result<Vec<Project>, MetaError> {
        self.blocking(projects::list_projects).await
    }

    async fn rename_project(
        &self,
        project_id: ProjectId,
        name: String,
    ) -> Result<Project, MetaError> {
        self.blocking(move |db| projects::rename_project(db, project_id, name))
            .await
    }

    async fn set_project_enabled(
        &self,
        project_id: ProjectId,
        enabled: bool,
    ) -> Result<Project, MetaError> {
        self.blocking(move |db| projects::set_project_enabled(db, project_id, enabled))
            .await
    }

    async fn delete_project(&self, project_id: ProjectId) -> Result<(), MetaError> {
        self.blocking(move |db| projects::delete_project(db, project_id))
            .await
    }

    async fn get_member(
        &self,
        project_id: ProjectId,
        user_id: UserId,
    ) -> Result<Option<ProjectMember>, MetaError> {
        self.blocking(move |db| projects::get_member(db, project_id, user_id))
            .await
    }

    async fn list_members(&self, project_id: ProjectId) -> Result<Vec<ProjectMember>, MetaError> {
        self.blocking(move |db| projects::list_members(db, project_id))
            .await
    }

    async fn list_user_memberships(
        &self,
        user_id: UserId,
    ) -> Result<Vec<ProjectMember>, MetaError> {
        self.blocking(move |db| projects::list_user_memberships(db, user_id))
            .await
    }

    async fn set_member_role(
        &self,
        project_id: ProjectId,
        user_id: UserId,
        role: ProjectRole,
    ) -> Result<ProjectMember, MetaError> {
        self.blocking(move |db| projects::set_member_role(db, project_id, user_id, role))
            .await
    }

    async fn remove_member(&self, project_id: ProjectId, user_id: UserId) -> Result<(), MetaError> {
        self.blocking(move |db| projects::remove_member(db, project_id, user_id))
            .await
    }

    async fn create_invite(&self, cmd: CreateInvite) -> Result<ProjectInvite, MetaError> {
        let resolved = ResolvedCreateInvite {
            cmd,
            invite_id: InviteId::new(),
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| projects::create_invite(db, resolved))
            .await
    }

    async fn get_invite(&self, invite_id: InviteId) -> Result<Option<ProjectInvite>, MetaError> {
        self.blocking(move |db| projects::get_invite(db, invite_id))
            .await
    }

    async fn list_project_invites(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectInvite>, MetaError> {
        self.blocking(move |db| projects::list_project_invites(db, project_id))
            .await
    }

    async fn list_invites(&self) -> Result<Vec<ProjectInvite>, MetaError> {
        self.blocking(projects::list_invites).await
    }

    async fn delete_invite(&self, invite_id: InviteId) -> Result<(), MetaError> {
        self.blocking(move |db| projects::delete_invite(db, invite_id))
            .await
    }

    async fn accept_invite(
        &self,
        invite_id: InviteId,
        user_id: UserId,
    ) -> Result<ProjectMember, MetaError> {
        let answer = ResolvedInviteAnswer {
            invite_id,
            user_id,
            accept: true,
            at: OffsetDateTime::now_utc(),
        };
        let member = self
            .blocking(move |db| projects::answer_invite(db, answer))
            .await?;
        member.ok_or_else(|| MetaError::Db("accepting an invite returned no member".into()))
    }

    async fn decline_invite(&self, invite_id: InviteId, user_id: UserId) -> Result<(), MetaError> {
        let answer = ResolvedInviteAnswer {
            invite_id,
            user_id,
            accept: false,
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| projects::answer_invite(db, answer))
            .await?;
        Ok(())
    }

    async fn put_project_credential(
        &self,
        credential: Credential,
        max_access_keys: u32,
    ) -> Result<(), MetaError> {
        self.blocking(move |db| projects::put_project_credential(db, credential, max_access_keys))
            .await
    }

    async fn set_credential_enabled(
        &self,
        access_key: &str,
        enabled: bool,
    ) -> Result<(), MetaError> {
        let access_key = access_key.to_string();
        self.blocking(move |db| projects::set_credential_enabled(db, &access_key, enabled))
            .await
    }

    async fn touch_credential(
        &self,
        access_key: &str,
        at: OffsetDateTime,
    ) -> Result<(), MetaError> {
        let touch = ResolvedTouchCredential {
            access_key: access_key.to_string(),
            at,
        };
        self.blocking(move |db| projects::touch_credential(db, touch))
            .await
    }

    async fn delete_session(&self, access_key: &str) -> Result<(), MetaError> {
        let access_key = access_key.to_string();
        self.blocking(move |db| projects::delete_session(db, &access_key))
            .await
    }

    async fn get_subscription(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<Subscription>, MetaError> {
        self.blocking(move |db| billing_apply::get_subscription(db, project_id))
            .await
    }

    async fn list_subscriptions(&self) -> Result<Vec<Subscription>, MetaError> {
        self.blocking(billing_apply::list_subscriptions).await
    }

    async fn pay_for_project(
        &self,
        project_id: ProjectId,
        payer: UserId,
        plan: Plan,
    ) -> Result<(Subscription, LedgerEntry), MetaError> {
        let pay = ResolvedPay {
            project_id,
            payer,
            plan,
            at: now(),
            entry_id: LedgerEntryId::new(),
        };
        self.blocking(move |db| billing_apply::pay_for_project(db, pay))
            .await
    }

    async fn set_subscription_cancelled(
        &self,
        project_id: ProjectId,
        user_id: UserId,
        cancel: bool,
    ) -> Result<Subscription, MetaError> {
        let change = ResolvedCancel {
            project_id,
            user_id,
            cancel,
            at: now(),
        };
        self.blocking(move |db| billing_apply::set_cancel(db, change))
            .await
    }

    async fn advance_subscription(
        &self,
        expected: &Subscription,
        timings: BillingTimings,
        at: OffsetDateTime,
    ) -> Result<Option<(Subscription, Step)>, MetaError> {
        let advance = ResolvedAdvance {
            project_id: expected.project_id,
            expected_state: expected.state,
            expected_period_end: expected.period_end,
            timings,
            at,
            entry_id: LedgerEntryId::new(),
        };
        self.blocking(move |db| billing_apply::advance(db, advance))
            .await
    }

    async fn get_usage(&self, owner: OwnerId) -> Result<OwnerUsage, MetaError> {
        self.blocking(move |db| billing_apply::get_usage(db, owner))
            .await
    }

    async fn check_write(&self, owner: OwnerId, additional: u64) -> Result<(), MetaError> {
        self.blocking(move |db| billing_apply::check_write(db, owner, additional))
            .await
    }

    async fn derived_indexes_built(&self) -> Result<bool, MetaError> {
        self.blocking(billing_apply::derived_indexes_built).await
    }

    async fn rebuild_derived_indexes(&self) -> Result<(), MetaError> {
        self.blocking(billing_apply::rebuild_derived_indexes).await
    }

    async fn get_balance(&self, user_id: UserId) -> Result<u64, MetaError> {
        self.blocking(move |db| billing_apply::get_balance(db, user_id))
            .await
    }

    async fn list_ledger(
        &self,
        user_id: UserId,
        limit: usize,
    ) -> Result<Vec<LedgerEntry>, MetaError> {
        self.blocking(move |db| billing_apply::list_ledger(db, user_id, limit))
            .await
    }

    async fn create_gift_code(&self, code: NewGiftCode) -> Result<GiftCode, MetaError> {
        let new = ResolvedNewGiftCode {
            code,
            code_id: GiftCodeId::new(),
            at: now(),
        };
        self.blocking(move |db| billing_apply::create_gift_code(db, new))
            .await
    }

    async fn list_gift_codes(&self) -> Result<Vec<GiftCode>, MetaError> {
        self.blocking(billing_apply::list_gift_codes).await
    }

    async fn set_gift_code_disabled(
        &self,
        code_id: GiftCodeId,
        disabled: bool,
    ) -> Result<GiftCode, MetaError> {
        self.blocking(move |db| billing_apply::set_gift_code_disabled(db, code_id, disabled))
            .await
    }

    async fn list_redemptions(
        &self,
        code_id: GiftCodeId,
    ) -> Result<Vec<GiftCodeRedemption>, MetaError> {
        self.blocking(move |db| billing_apply::list_redemptions(db, code_id))
            .await
    }

    async fn redeem_gift_code(
        &self,
        code_hash: String,
        user_id: UserId,
    ) -> Result<(GiftCode, LedgerEntry), MetaError> {
        let redeem = ResolvedRedeem {
            code_hash,
            user_id,
            at: now(),
            entry_id: LedgerEntryId::new(),
        };
        self.blocking(move |db| billing_apply::redeem(db, redeem))
            .await
    }

    async fn create_checkout(&self, checkout: NewCheckout) -> Result<Checkout, MetaError> {
        let new = ResolvedNewCheckout {
            checkout,
            checkout_id: CheckoutId::new(),
            at: now(),
        };
        self.blocking(move |db| billing_apply::create_checkout(db, new))
            .await
    }

    async fn get_checkout(&self, checkout_id: CheckoutId) -> Result<Option<Checkout>, MetaError> {
        self.blocking(move |db| billing_apply::get_checkout(db, checkout_id))
            .await
    }

    async fn complete_checkout(
        &self,
        checkout_id: CheckoutId,
        succeeded: bool,
        provider_ref: Option<String>,
    ) -> Result<Checkout, MetaError> {
        let done = ResolvedCompleteCheckout {
            checkout_id,
            succeeded,
            provider_ref,
            at: now(),
            entry_id: LedgerEntryId::new(),
        };
        self.blocking(move |db| billing_apply::complete_checkout(db, done))
            .await
    }

    async fn get_user(&self, user_id: UserId) -> Result<Option<User>, MetaError> {
        self.blocking(move |db| applier::get_user(db, user_id))
            .await
    }

    async fn list_users(&self) -> Result<Vec<User>, MetaError> {
        self.blocking(applier::list_users).await
    }

    async fn set_user_access(
        &self,
        user_id: UserId,
        access: UserAccess,
    ) -> Result<User, MetaError> {
        self.blocking(move |db| applier::set_user_access(db, user_id, access))
            .await
    }

    async fn relink_user(
        &self,
        from: UserId,
        into: UserId,
        actor: String,
    ) -> Result<User, MetaError> {
        let resolved = crate::raft::types::ResolvedRelinkUser {
            from,
            into,
            actor,
            entry_id: loony_core::LedgerEntryId::new(),
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| crate::raft::accounts::relink_user(db, resolved))
            .await
    }

    async fn make_project_owner(
        &self,
        project_id: ProjectId,
        user_id: UserId,
    ) -> Result<ProjectMember, MetaError> {
        let resolved = crate::raft::types::ResolvedMakeOwner {
            project_id,
            user_id,
            at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| crate::raft::accounts::make_project_owner(db, resolved))
            .await
    }

    async fn list_buckets(&self, owner: OwnerId) -> Result<Vec<Bucket>, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(BUCKETS).map_err(db_err)?;
            let mut result = Vec::new();
            for entry in table.iter().map_err(db_err)? {
                let (_key, value) = entry.map_err(db_err)?;
                let bucket: Bucket = serde_json::from_slice(value.value())?;
                if bucket.owner_id == owner {
                    result.push(bucket);
                }
            }
            result.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            Ok(result)
        })
        .await
    }

    async fn commit_manifest(&self, manifest: ObjectManifest) -> Result<ObjectManifest, MetaError> {
        self.blocking(move |db| applier::commit_manifest(db, manifest))
            .await
    }

    async fn get_manifest(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectManifest>, MetaError> {
        let key = key.clone();
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(OBJECTS).map_err(db_err)?;
            let row_key = object_row_key(bucket_id, &key);
            match table.get(row_key.as_str()).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn tombstone_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<(), MetaError> {
        let key = key.clone();
        self.blocking(move |db| applier::tombstone_object(db, bucket_id, &key))
            .await
    }

    async fn list_objects(&self, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(OBJECTS).map_err(db_err)?;

            let bucket_prefix = object_row_prefix(query.bucket_id);
            let key_prefix = query.prefix.clone().unwrap_or_default();
            let full_prefix = format!("{bucket_prefix}{key_prefix}");

            // `continuation_token` is our own opaque cursor (see `cursor.rs`): it encodes
            // the key of the first not-yet-returned item, so resuming from it means
            // ranging from it inclusively with no further skipping. `start_after` is a
            // client-supplied literal key with the upstream protocol's exclusive-lower-bound semantics,
            // so it ranges from the same point but the boundary row itself is skipped.
            let start_after_boundary = query
                .start_after
                .clone()
                .map(|k| format!("{bucket_prefix}{k}"));
            let (range_start, exclusive_boundary): (String, Option<String>) =
                match &query.continuation_token {
                    Some(token) => {
                        let key = crate::cursor::decode(token)
                            .ok_or(MetaError::InvalidContinuationToken)?;
                        // Never resume before the requested prefix, whatever the token says.
                        (
                            format!("{bucket_prefix}{key}").max(full_prefix.clone()),
                            None,
                        )
                    }
                    None => match &start_after_boundary {
                        Some(boundary) => (boundary.clone(), Some(boundary.clone())),
                        None => (full_prefix.clone(), None),
                    },
                };
            let iter = table.range(range_start.as_str()..).map_err(db_err)?;

            let mut objects = Vec::new();
            let mut common_prefixes: Vec<String> = Vec::new();
            let mut is_truncated = false;
            let mut next_token = None;
            let max_keys = query.max_keys.max(1) as usize;

            for entry in iter {
                let (key_guard, value_guard) = entry.map_err(db_err)?;
                let row_key = key_guard.value().to_string();

                if !row_key.starts_with(&full_prefix) {
                    break; // sorted keys: past this bucket/prefix means we're done
                }
                if let Some(boundary) = &exclusive_boundary
                    && row_key.as_str() <= boundary.as_str()
                {
                    continue;
                }

                let logical_key = &row_key[bucket_prefix.len()..];

                if let Some(delim) = &query.delimiter {
                    let after_prefix = &logical_key[key_prefix.len()..];
                    if let Some(idx) = after_prefix.find(delim.as_str()) {
                        let common = format!("{key_prefix}{}", &after_prefix[..idx + delim.len()]);
                        if common_prefixes.last() != Some(&common) {
                            if objects.len() + common_prefixes.len() >= max_keys {
                                is_truncated = true;
                                next_token = Some(crate::cursor::encode(logical_key));
                                break;
                            }
                            common_prefixes.push(common);
                        }
                        continue;
                    }
                }

                if objects.len() + common_prefixes.len() >= max_keys {
                    is_truncated = true;
                    next_token = Some(crate::cursor::encode(logical_key));
                    break;
                }

                let manifest: ObjectManifest = serde_json::from_slice(value_guard.value())?;
                objects.push(ObjectSummary {
                    key: manifest.key.clone(),
                    size: manifest.total_size(),
                    etag: manifest.etag.clone(),
                    last_modified: manifest.created_at,
                });
            }

            Ok(ListObjectsPage {
                objects,
                common_prefixes,
                is_truncated,
                next_continuation_token: next_token,
            })
        })
        .await
    }

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError> {
        let resolved = ResolvedBeginMultipart {
            cmd,
            upload_id: UploadId::new(),
            initiated_at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| applier::begin_multipart(db, resolved))
            .await
    }

    async fn get_upload(
        &self,
        upload_id: UploadId,
    ) -> Result<Option<MultipartUploadState>, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(MULTIPART).map_err(db_err)?;
            match table.get(upload_id.to_string().as_str()).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn record_part(&self, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError> {
        self.blocking(move |db| applier::record_part(db, upload_id, part))
            .await
    }

    async fn list_parts(&self, upload_id: UploadId) -> Result<Vec<PartSummary>, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(MULTIPART).map_err(db_err)?;
            let key = upload_id.to_string();
            let guard = table
                .get(key.as_str())
                .map_err(db_err)?
                .ok_or_else(|| MetaError::NoSuchUpload(upload_id.to_string()))?;
            let state: MultipartUploadState = serde_json::from_slice(guard.value())?;
            Ok(state
                .parts
                .into_values()
                .map(|p| PartSummary {
                    part_number: p.part_number,
                    size: p.size,
                    etag_md5: p.etag_md5,
                })
                .collect())
        })
        .await
    }

    async fn complete_multipart(
        &self,
        cmd: CompleteMultipart,
    ) -> Result<ObjectManifest, MetaError> {
        let resolved = ResolvedCompleteMultipart {
            cmd,
            object_id: ObjectId::new(),
            version_id: VersionId::new(),
            created_at: OffsetDateTime::now_utc(),
        };
        self.blocking(move |db| applier::complete_multipart(db, resolved))
            .await
    }

    async fn abort_multipart(&self, upload_id: UploadId) -> Result<(), MetaError> {
        self.blocking(move |db| applier::abort_multipart(db, upload_id))
            .await
    }

    async fn put_credential(&self, cred: Credential) -> Result<(), MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
                let bytes = serde_json::to_vec(&cred)?;
                table
                    .insert(cred.access_key.as_str(), bytes.as_slice())
                    .map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_credential(&self, access_key: &str) -> Result<Option<Credential>, MetaError> {
        let access_key = access_key.to_string();
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(CREDENTIALS).map_err(db_err)?;
            match table.get(access_key.as_str()).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn bootstrap_cluster(&self, cluster_id: ClusterId) -> Result<(), MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(CLUSTER).map_err(db_err)?;
                let existing: Option<ClusterId> = match table.get(CLUSTER_ID_KEY).map_err(db_err)? {
                    Some(guard) => Some(serde_json::from_slice(guard.value())?),
                    None => None,
                };
                match existing {
                    Some(existing) if existing != cluster_id => {
                        return Err(MetaError::ClusterIdMismatch {
                            existing: existing.to_string(),
                            requested: cluster_id.to_string(),
                        });
                    }
                    Some(_) => {}
                    None => {
                        let bytes = serde_json::to_vec(&cluster_id)?;
                        table
                            .insert(CLUSTER_ID_KEY, bytes.as_slice())
                            .map_err(db_err)?;
                    }
                }
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_cluster_id(&self) -> Result<Option<ClusterId>, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(CLUSTER).map_err(db_err)?;
            match table.get(CLUSTER_ID_KEY).map_err(db_err)? {
                Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn register_node(&self, cmd: RegisterNode) -> Result<NodeInfo, MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            let info = {
                let mut table = write_txn.open_table(NODES).map_err(db_err)?;
                let key = cmd.node_id.to_string();
                let existing: Option<NodeInfo> = match table.get(key.as_str()).map_err(db_err)? {
                    Some(guard) => Some(serde_json::from_slice(guard.value())?),
                    None => None,
                };
                let state =
                    applier::registration_state(cmd.node_id, existing.as_ref().map(|n| n.state))?;
                let generation = existing.map_or(0, |n| n.generation + 1);
                let info = NodeInfo {
                    node_id: cmd.node_id,
                    advertised_address: cmd.advertised_address,
                    state,
                    generation,
                    last_seen: OffsetDateTime::now_utc(),
                    failure_domain: cmd.failure_domain,
                    volumes: cmd.volumes,
                };
                let bytes = serde_json::to_vec(&info)?;
                table
                    .insert(key.as_str(), bytes.as_slice())
                    .map_err(db_err)?;
                info
            };
            write_txn.commit().map_err(db_err)?;
            Ok(info)
        })
        .await
    }

    async fn update_node_state(&self, node_id: NodeId, state: NodeState) -> Result<(), MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(NODES).map_err(db_err)?;
                let key = node_id.to_string();
                let mut info: NodeInfo = match table.get(key.as_str()).map_err(db_err)? {
                    Some(guard) => serde_json::from_slice(guard.value())?,
                    None => return Err(MetaError::NoSuchNode(key)),
                };
                applier::check_node_transition(node_id, info.state, state)?;
                info.state = state;
                info.last_seen = OffsetDateTime::now_utc();
                let bytes = serde_json::to_vec(&info)?;
                table
                    .insert(key.as_str(), bytes.as_slice())
                    .map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(NODES).map_err(db_err)?;
            let mut result = Vec::new();
            for entry in table.iter().map_err(db_err)? {
                let (_key, value) = entry.map_err(db_err)?;
                result.push(serde_json::from_slice(value.value())?);
            }
            result.sort_by(|a: &NodeInfo, b: &NodeInfo| {
                a.node_id.to_string().cmp(&b.node_id.to_string())
            });
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loony_core::ETag;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    use loony_core::{DurabilityPolicy, NodeId, ShardLocation, Stripe, VolumeId};

    async fn store() -> RedbMetadataStore {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so it outlives the store for the duration of the test
        // process; each test gets its own directory so there's no cross-test sharing.
        let path = dir.keep().join("metadata.redb");
        RedbMetadataStore::open(path).await.unwrap()
    }

    fn shard(index: u16) -> ShardLocation {
        ShardLocation {
            shard_index: index,
            node_id: NodeId::new(),
            volume_id: VolumeId::new(),
            shard_id: loony_core::ShardId::new(),
            size: 3,
            checksum: [index as u8; 32],
            generation: 0,
        }
    }

    fn manifest(bucket_id: BucketId, key: &str) -> ObjectManifest {
        ObjectManifest {
            object_id: ObjectId::new(),
            bucket_id,
            key: ObjectKey::parse(key).unwrap(),
            version_id: VersionId::new(),
            size: 3,
            etag: ETag::from_md5([9u8; 16]),
            sha256: [7u8; 32],
            content_type: "application/octet-stream".into(),
            user_metadata: Default::default(),
            created_at: OffsetDateTime::now_utc(),
            delete_marker: false,
            parts: vec![PartManifest {
                part_number: 1,
                offset: 0,
                size: 3,
                etag_md5: [9u8; 16],
                stripes: vec![Stripe {
                    stripe_index: 0,
                    stripe_offset: 0,
                    stripe_len: 3,
                    durability: DurabilityPolicy::Replicated { n: 1 },
                    shards: vec![shard(0)],
                }],
            }],
        }
    }

    #[tokio::test]
    async fn create_get_and_list_buckets() {
        let store = store().await;
        let owner = OwnerId::new();

        let bucket = store
            .create_bucket(CreateBucket {
                name: BucketName::parse("my-bucket").unwrap(),
                owner_id: owner,
                region: "us-east-1".into(),
            })
            .await
            .unwrap();

        let fetched = store.get_bucket(&bucket.name).await.unwrap().unwrap();
        assert_eq!(fetched.bucket_id, bucket.bucket_id);

        let listed = store.list_buckets(owner).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].bucket_id, bucket.bucket_id);

        let other_owner_listed = store.list_buckets(OwnerId::new()).await.unwrap();
        assert!(other_owner_listed.is_empty());
    }

    #[tokio::test]
    async fn create_bucket_is_rejected_when_name_taken() {
        let store = store().await;
        let cmd = CreateBucket {
            name: BucketName::parse("dup").unwrap(),
            owner_id: OwnerId::new(),
            region: "us-east-1".into(),
        };
        store.create_bucket(cmd.clone()).await.unwrap();
        let err = store.create_bucket(cmd).await.unwrap_err();
        assert!(matches!(err, MetaError::BucketAlreadyExists(_)));
    }

    #[tokio::test]
    async fn delete_bucket_requires_it_to_be_empty() {
        let store = store().await;
        let bucket = store
            .create_bucket(CreateBucket {
                name: BucketName::parse("has-objects").unwrap(),
                owner_id: OwnerId::new(),
                region: "us-east-1".into(),
            })
            .await
            .unwrap();

        store
            .commit_manifest(manifest(bucket.bucket_id, "a"))
            .await
            .unwrap();

        let err = store.delete_bucket(&bucket.name).await.unwrap_err();
        assert!(matches!(err, MetaError::BucketNotEmpty(_)));

        store
            .tombstone_object(bucket.bucket_id, &ObjectKey::parse("a").unwrap())
            .await
            .unwrap();
        store.delete_bucket(&bucket.name).await.unwrap();
        assert!(store.get_bucket(&bucket.name).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_bucket_rejects_unknown_name() {
        let store = store().await;
        let err = store
            .delete_bucket(&BucketName::parse("nope").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NoSuchBucket(_)));
    }

    #[tokio::test]
    async fn commit_get_and_tombstone_manifest_roundtrip() {
        let store = store().await;
        let bucket_id = BucketId::new();
        let key = ObjectKey::parse("a/b/c").unwrap();

        assert!(store.get_manifest(bucket_id, &key).await.unwrap().is_none());

        let committed = store
            .commit_manifest(manifest(bucket_id, "a/b/c"))
            .await
            .unwrap();
        let fetched = store.get_manifest(bucket_id, &key).await.unwrap().unwrap();
        assert_eq!(fetched.object_id, committed.object_id);

        // A second PUT to the same key overwrites what GET returns (Phase 2/3 scope:
        // only the current version is retained; full history is Phase 11).
        let second = store
            .commit_manifest(manifest(bucket_id, "a/b/c"))
            .await
            .unwrap();
        let fetched = store.get_manifest(bucket_id, &key).await.unwrap().unwrap();
        assert_eq!(fetched.object_id, second.object_id);
        assert_ne!(second.object_id, committed.object_id);

        store.tombstone_object(bucket_id, &key).await.unwrap();
        assert!(store.get_manifest(bucket_id, &key).await.unwrap().is_none());

        // Idempotent: tombstoning an already-gone object is not an error.
        store.tombstone_object(bucket_id, &key).await.unwrap();
    }

    #[tokio::test]
    async fn list_objects_respects_prefix_delimiter_and_pagination() {
        let store = store().await;
        let bucket_id = BucketId::new();
        for key in [
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "photos/2025/c.jpg",
            "docs/readme.md",
        ] {
            store
                .commit_manifest(manifest(bucket_id, key))
                .await
                .unwrap();
        }

        // No delimiter: every key under the prefix comes back flat.
        let page = store
            .list_objects(ListObjectsQuery {
                bucket_id,
                prefix: Some("photos/".into()),
                delimiter: None,
                start_after: None,
                continuation_token: None,
                max_keys: 100,
            })
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 3);
        assert!(page.common_prefixes.is_empty());
        assert!(!page.is_truncated);

        // With a delimiter, same-prefix keys collapse into one common prefix each.
        let page = store
            .list_objects(ListObjectsQuery {
                bucket_id,
                prefix: Some("photos/".into()),
                delimiter: Some("/".into()),
                start_after: None,
                continuation_token: None,
                max_keys: 100,
            })
            .await
            .unwrap();
        assert!(page.objects.is_empty());
        assert_eq!(page.common_prefixes, vec!["photos/2024/", "photos/2025/"]);

        // Pagination: max_keys=1 truncates and hands back a token that resumes cleanly.
        let first_page = store
            .list_objects(ListObjectsQuery {
                bucket_id,
                prefix: None,
                delimiter: None,
                start_after: None,
                continuation_token: None,
                max_keys: 1,
            })
            .await
            .unwrap();
        assert_eq!(first_page.objects.len(), 1);
        assert!(first_page.is_truncated);
        let token = first_page.next_continuation_token.clone().unwrap();

        let second_page = store
            .list_objects(ListObjectsQuery {
                bucket_id,
                prefix: None,
                delimiter: None,
                start_after: None,
                continuation_token: Some(token),
                max_keys: 100,
            })
            .await
            .unwrap();
        assert_eq!(
            first_page.objects[0].key,
            ObjectKey::parse("docs/readme.md").unwrap()
        );
        assert_eq!(second_page.objects.len(), 3);
        assert!(!second_page.is_truncated);
    }

    #[tokio::test]
    async fn multipart_upload_lifecycle() {
        let store = store().await;
        let bucket_id = BucketId::new();
        let key = ObjectKey::parse("big-file.bin").unwrap();

        let upload_id = store
            .begin_multipart(BeginMultipart {
                bucket_id,
                key: key.clone(),
                content_type: "application/octet-stream".into(),
                user_metadata: Default::default(),
            })
            .await
            .unwrap();

        let part1 = PartManifest {
            part_number: 1,
            offset: 0,
            size: 5,
            etag_md5: [1u8; 16],
            stripes: vec![Stripe {
                stripe_index: 0,
                stripe_offset: 0,
                stripe_len: 5,
                durability: DurabilityPolicy::Replicated { n: 1 },
                shards: vec![shard(0)],
            }],
        };
        let part2 = PartManifest {
            part_number: 2,
            offset: 0,
            size: 7,
            etag_md5: [2u8; 16],
            stripes: vec![Stripe {
                stripe_index: 0,
                stripe_offset: 0,
                stripe_len: 7,
                durability: DurabilityPolicy::Replicated { n: 1 },
                shards: vec![shard(0)],
            }],
        };
        store.record_part(upload_id, part1.clone()).await.unwrap();
        store.record_part(upload_id, part2.clone()).await.unwrap();

        let parts = store.list_parts(upload_id).await.unwrap();
        assert_eq!(parts.len(), 2);

        let etag1 = hex(&part1.etag_md5);
        let etag2 = hex(&part2.etag_md5);
        let manifest = store
            .complete_multipart(CompleteMultipart {
                upload_id,
                requested_parts: vec![(1, etag1), (2, etag2)],
                content_type: "application/octet-stream".into(),
            })
            .await
            .unwrap();

        assert_eq!(manifest.size, 12);
        assert_eq!(manifest.parts[1].offset, 5);
        assert!(manifest.etag.as_str().ends_with("-2"));

        let fetched = store.get_manifest(bucket_id, &key).await.unwrap().unwrap();
        assert_eq!(fetched.object_id, manifest.object_id);

        // The upload record is gone after completion.
        let err = store.list_parts(upload_id).await.unwrap_err();
        assert!(matches!(err, MetaError::NoSuchUpload(_)));
    }

    #[tokio::test]
    async fn complete_multipart_rejects_mismatched_or_misordered_parts() {
        let store = store().await;
        let bucket_id = BucketId::new();
        let upload_id = store
            .begin_multipart(BeginMultipart {
                bucket_id,
                key: ObjectKey::parse("f").unwrap(),
                content_type: "application/octet-stream".into(),
                user_metadata: Default::default(),
            })
            .await
            .unwrap();

        let part1 = PartManifest {
            part_number: 1,
            offset: 0,
            size: 5,
            etag_md5: [1u8; 16],
            stripes: vec![],
        };
        let part2 = PartManifest {
            part_number: 2,
            offset: 0,
            size: 5,
            etag_md5: [2u8; 16],
            stripes: vec![],
        };
        store.record_part(upload_id, part1.clone()).await.unwrap();
        store.record_part(upload_id, part2.clone()).await.unwrap();

        let wrong_etag = store
            .complete_multipart(CompleteMultipart {
                upload_id,
                requested_parts: vec![(1, "deadbeef".repeat(4))],
                content_type: "application/octet-stream".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(wrong_etag, MetaError::InvalidPart));

        let out_of_order = store
            .complete_multipart(CompleteMultipart {
                upload_id,
                requested_parts: vec![(2, hex(&part2.etag_md5)), (1, hex(&part1.etag_md5))],
                content_type: "application/octet-stream".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(out_of_order, MetaError::InvalidPartOrder));

        let missing_part = store
            .complete_multipart(CompleteMultipart {
                upload_id,
                requested_parts: vec![(1, hex(&part1.etag_md5)), (3, "x".into())],
                content_type: "application/octet-stream".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(missing_part, MetaError::InvalidPart));
    }

    #[tokio::test]
    async fn abort_multipart_is_idempotent_and_discards_recorded_parts() {
        let store = store().await;
        let upload_id = store
            .begin_multipart(BeginMultipart {
                bucket_id: BucketId::new(),
                key: ObjectKey::parse("f").unwrap(),
                content_type: "application/octet-stream".into(),
                user_metadata: Default::default(),
            })
            .await
            .unwrap();

        store.abort_multipart(upload_id).await.unwrap();
        store.abort_multipart(upload_id).await.unwrap(); // idempotent

        let err = store.list_parts(upload_id).await.unwrap_err();
        assert!(matches!(err, MetaError::NoSuchUpload(_)));
    }

    #[tokio::test]
    async fn credentials_roundtrip() {
        let store = store().await;
        assert!(store.get_credential("AKIA_TEST").await.unwrap().is_none());

        let cred = Credential {
            access_key: "AKIA_TEST".into(),
            secret_key: "shh".into(),
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
            created_at: OffsetDateTime::now_utc(),
        };
        store.put_credential(cred.clone()).await.unwrap();

        let fetched = store.get_credential("AKIA_TEST").await.unwrap().unwrap();
        assert_eq!(fetched.secret_key, "shh");
        assert_eq!(fetched.owner_id, cred.owner_id);
    }

    #[tokio::test]
    async fn state_survives_reopening_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("metadata.redb");

        let bucket_id;
        {
            let store = RedbMetadataStore::open(&path).await.unwrap();
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

        let store = RedbMetadataStore::open(&path).await.unwrap();
        let fetched = store
            .get_bucket(&BucketName::parse("persisted").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.bucket_id, bucket_id);
    }

    #[tokio::test]
    async fn bootstrap_cluster_is_idempotent_and_rejects_mismatched_ids() {
        let store = store().await;
        assert_eq!(store.get_cluster_id().await.unwrap(), None);

        let id = loony_core::ClusterId::parse("cluster-one").unwrap();
        store.bootstrap_cluster(id.clone()).await.unwrap();
        assert_eq!(store.get_cluster_id().await.unwrap(), Some(id.clone()));

        // Calling it again with the same id is a no-op, not an error.
        store.bootstrap_cluster(id.clone()).await.unwrap();

        let other = loony_core::ClusterId::parse("cluster-two").unwrap();
        let err = store.bootstrap_cluster(other).await.unwrap_err();
        assert!(matches!(err, MetaError::ClusterIdMismatch { .. }));
        // The mismatch attempt didn't change anything.
        assert_eq!(store.get_cluster_id().await.unwrap(), Some(id));
    }

    #[tokio::test]
    async fn register_node_bumps_generation_on_rejoin() {
        let store = store().await;
        let node_id = loony_core::NodeId::new();

        let first = store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec!["rack:a".into()],
                volumes: vec![],
            })
            .await
            .unwrap();
        assert_eq!(first.generation, 0);
        assert_eq!(first.state, loony_core::NodeState::Joining);

        let second = store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec!["rack:a".into()],
                volumes: vec![],
            })
            .await
            .unwrap();
        assert_eq!(second.generation, 1);
    }

    #[tokio::test]
    async fn update_node_state_requires_prior_registration() {
        let store = store().await;
        let node_id = loony_core::NodeId::new();

        let err = store
            .update_node_state(node_id, loony_core::NodeState::Healthy)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NoSuchNode(_)));

        store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec![],
                volumes: vec![],
            })
            .await
            .unwrap();
        store
            .update_node_state(node_id, loony_core::NodeState::Healthy)
            .await
            .unwrap();

        let nodes = store.list_nodes().await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].state, loony_core::NodeState::Healthy);
    }

    #[tokio::test]
    async fn list_nodes_returns_every_registered_node() {
        let store = store().await;
        for i in 0..3 {
            store
                .register_node(RegisterNode {
                    node_id: loony_core::NodeId::new(),
                    advertised_address: format!("node-{i}:9100"),
                    failure_domain: vec![],
                    volumes: vec![],
                })
                .await
                .unwrap();
        }
        assert_eq!(store.list_nodes().await.unwrap().len(), 3);
    }

    fn register_cmd(node_id: NodeId) -> RegisterNode {
        RegisterNode {
            node_id,
            advertised_address: "node-a:9100".into(),
            failure_domain: vec![],
            volumes: vec![],
        }
    }

    #[tokio::test]
    async fn operator_node_states_survive_automation_and_restarts() {
        use loony_core::NodeState::{Draining, Healthy, Joining, Removed};
        let store = store().await;
        let node_id = NodeId::new();
        let state = async |store: &RedbMetadataStore| store.list_nodes().await.unwrap()[0].state;

        store.register_node(register_cmd(node_id)).await.unwrap();
        store.update_node_state(node_id, Healthy).await.unwrap();

        // Only a drained node can be removed.
        assert!(matches!(
            store.update_node_state(node_id, Removed).await.unwrap_err(),
            MetaError::InvalidNodeTransition { .. }
        ));

        store.update_node_state(node_id, Draining).await.unwrap();

        // A heartbeat that started before the drain can't undo it...
        assert!(store.update_node_state(node_id, Healthy).await.is_err());
        assert_eq!(state(&store).await, Draining);
        // ...and neither can the node restarting and re-registering.
        store.register_node(register_cmd(node_id)).await.unwrap();
        assert_eq!(state(&store).await, Draining);

        // Resuming is explicit, and heartbeats take it from there.
        store.update_node_state(node_id, Joining).await.unwrap();
        store.update_node_state(node_id, Healthy).await.unwrap();

        store.update_node_state(node_id, Draining).await.unwrap();
        store.update_node_state(node_id, Removed).await.unwrap();

        // Removal is final.
        assert!(store.update_node_state(node_id, Joining).await.is_err());
        assert!(matches!(
            store
                .register_node(register_cmd(node_id))
                .await
                .unwrap_err(),
            MetaError::NodeRemoved(_)
        ));
    }

    #[tokio::test]
    async fn credentials_can_be_listed_and_deleted() {
        let store = store().await;
        let cred = Credential {
            access_key: "AKIA_LIST".into(),
            secret_key: "shh".into(),
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
            created_at: OffsetDateTime::now_utc(),
        };
        store.put_credential(cred).await.unwrap();
        assert_eq!(store.list_credentials().await.unwrap().len(), 1);

        store.delete_credential("AKIA_LIST").await.unwrap();
        store.delete_credential("AKIA_LIST").await.unwrap(); // idempotent
        assert!(store.list_credentials().await.unwrap().is_empty());
    }

    #[test]
    fn a_credential_stored_before_the_admin_flag_existed_is_not_admin() {
        let json = format!(
            r#"{{"access_key":"a","secret_key":"s","owner_id":"{}","enabled":true,"created_at":"2026-01-01T00:00:00Z"}}"#,
            OwnerId::new()
        );
        assert!(!serde_json::from_str::<Credential>(&json).unwrap().admin);
    }

    #[test]
    fn a_credential_stored_before_these_fields_existed_is_a_standard_key_that_never_expires() {
        let json = r#"{"access_key":"A","secret_key":"S","owner_id":"00000000-0000-7000-8000-000000000000","enabled":true,"created_at":"2025-01-01T00:00:00Z"}"#;
        let credential = serde_json::from_str::<Credential>(json).unwrap();
        assert_eq!(credential.kind, crate::commands::CredentialKind::Standard);
        assert_eq!(credential.user_id, None);
        assert_eq!(credential.expires_at, None);
        assert!(!credential.is_expired(OffsetDateTime::now_utc()));
    }

    #[test]
    fn a_credential_is_expired_from_its_expiry_onward() {
        let json = r#"{"access_key":"A","secret_key":"S","owner_id":"00000000-0000-7000-8000-000000000000","enabled":true,"expires_at":"2025-01-01T12:00:00Z","created_at":"2025-01-01T00:00:00Z"}"#;
        let credential = serde_json::from_str::<Credential>(json).unwrap();
        let expiry = credential.expires_at.unwrap();
        assert!(!credential.is_expired(expiry - time::Duration::seconds(1)));
        assert!(credential.is_expired(expiry));
    }
}
