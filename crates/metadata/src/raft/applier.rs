//! The Raft state machine's actual business logic: synchronous functions applying one
//! [`MetadataCommand`] (or serving one read) against a redb [`Database`], identical in
//! shape to `redb_store.rs`'s per-method closures because they *are* the same
//! computation — Phase 8 wraps the Phase 2 state machine in `openraft` rather than
//! reimplementing it (architecture.md §5, `lib.rs`'s module doc).
//!
//! Kept synchronous and `Database`-parameterized (not `self`-methods on a store type) so
//! both [`crate::raft::state_machine::RedbStateMachineStore::apply`] (inside
//! `spawn_blocking`, one call per committed log entry) and
//! [`crate::raft::store::RaftMetadataStore`]'s read methods (which bypass Raft
//! entirely — architecture.md §5's control-plane-vs-data-plane split doesn't apply to
//! reads of already-committed state) can call the exact same code.

use loony_core::{
    Bucket, BucketId, BucketName, ClusterId, ETag, NodeId, NodeInfo, NodeState, ObjectKey,
    ObjectManifest, OwnerId, UploadId, VersioningState,
};
use md5::{Digest as _, Md5};
use redb::{Database, ReadableTable, TableDefinition};
use sha2::Sha256;
use time::OffsetDateTime;

use crate::commands::{
    Credential, CredentialKind, ListObjectsPage, ListObjectsQuery, MultipartUploadState,
    ObjectSummary, PartSummary, RegisterNode, StorageStats, User, UserAccess,
};
use crate::error::MetaError;
use crate::raft::billing_apply;
use crate::raft::projects;
use crate::raft::types::{
    CommandResponse, MetadataCommand, ResolvedBeginMultipart, ResolvedCompleteMultipart,
    ResolvedCreateBucket, ResolvedUserSignIn,
};
use loony_core::{PartManifest, UserId};

pub(crate) const BUCKETS: TableDefinition<&str, &[u8]> = TableDefinition::new("buckets");
pub(crate) const OBJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("objects");
pub(crate) const MULTIPART: TableDefinition<&str, &[u8]> =
    TableDefinition::new("multipart_uploads");
pub(crate) const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
pub(crate) const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
/// `issuer\0subject` to the user's id, so a sign-in can find the user it belongs to.
pub(crate) const USER_SUBJECTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("user_subjects");
pub(crate) const PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
/// Keyed by `project\0user`.
pub(crate) const PROJECT_MEMBERS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("project_members");
pub(crate) const PROJECT_INVITES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("project_invites");
pub(crate) const NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
pub(crate) const CLUSTER: TableDefinition<&str, &[u8]> = TableDefinition::new("cluster");
pub(crate) const CLUSTER_ID_KEY: &str = "cluster_id";

/// Every business-data table the state machine owns — used to open/create them upfront
/// and to enumerate them wholesale for snapshotting (`snapshot.rs`).
pub(crate) const ALL_TABLES: &[TableDefinition<'static, &'static str, &'static [u8]>] = &[
    BUCKETS,
    OBJECTS,
    MULTIPART,
    CREDENTIALS,
    USERS,
    USER_SUBJECTS,
    PROJECTS,
    PROJECT_MEMBERS,
    PROJECT_INVITES,
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
    NODES,
    CLUSTER,
];

pub(crate) fn db_err<E: std::fmt::Display>(e: E) -> MetaError {
    MetaError::Db(e.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn object_row_prefix(bucket_id: BucketId) -> String {
    format!("{bucket_id}\0")
}

fn object_row_key(bucket_id: BucketId, key: &ObjectKey) -> String {
    format!("{bucket_id}\0{key}")
}

/// Dispatches one committed [`MetadataCommand`] to its handler, wrapping the whole
/// thing in a single redb write transaction — the same "transaction boundary is the
/// concurrency-control mechanism" discipline `redb_store.rs` documents.
pub(crate) fn apply_metadata_command(
    db: &Database,
    cmd: MetadataCommand,
) -> Result<CommandResponse, MetaError> {
    match cmd {
        MetadataCommand::CreateBucket(cmd) => {
            create_bucket(db, cmd).map(|b| CommandResponse::Bucket(Box::new(b)))
        }
        MetadataCommand::DeleteBucket(name) => {
            delete_bucket(db, &name).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::CommitManifest(manifest) => {
            commit_manifest(db, *manifest).map(|m| CommandResponse::Manifest(Box::new(m)))
        }
        MetadataCommand::TombstoneObject(bucket_id, key) => {
            tombstone_object(db, bucket_id, &key).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::BeginMultipart(cmd) => {
            begin_multipart(db, cmd).map(CommandResponse::UploadId)
        }
        MetadataCommand::RecordPart(upload_id, part) => {
            record_part(db, upload_id, *part).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::CompleteMultipart(cmd) => {
            complete_multipart(db, *cmd).map(|m| CommandResponse::Manifest(Box::new(m)))
        }
        MetadataCommand::AbortMultipart(upload_id) => {
            abort_multipart(db, upload_id).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::PutCredential(cred) => {
            put_credential(db, *cred).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::DeleteCredential(access_key) => {
            delete_credential(db, &access_key).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::RecordUserSignIn(resolved) => {
            record_user_sign_in(db, *resolved).map(|u| CommandResponse::User(Box::new(u)))
        }
        MetadataCommand::SetUserAccess(user_id, access) => {
            set_user_access(db, user_id, access).map(|u| CommandResponse::User(Box::new(u)))
        }
        MetadataCommand::CreateProject(resolved) => {
            projects::create_project(db, *resolved).map(|p| CommandResponse::Project(Box::new(p)))
        }
        MetadataCommand::AdoptOwner(resolved) => {
            projects::adopt_owner(db, *resolved).map(|p| CommandResponse::Project(Box::new(p)))
        }
        MetadataCommand::RenameProject(id, name) => {
            projects::rename_project(db, id, name).map(|p| CommandResponse::Project(Box::new(p)))
        }
        MetadataCommand::SetProjectEnabled(id, enabled) => {
            projects::set_project_enabled(db, id, enabled)
                .map(|p| CommandResponse::Project(Box::new(p)))
        }
        MetadataCommand::DeleteProject(id) => {
            projects::delete_project(db, id).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::SetMemberRole(project, user, role) => {
            projects::set_member_role(db, project, user, role)
                .map(|m| CommandResponse::Member(Box::new(m)))
        }
        MetadataCommand::RemoveMember(project, user) => {
            projects::remove_member(db, project, user).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::CreateInvite(resolved) => {
            projects::create_invite(db, *resolved).map(|i| CommandResponse::Invite(Box::new(i)))
        }
        MetadataCommand::DeleteInvite(id) => {
            projects::delete_invite(db, id).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::AnswerInvite(resolved) => {
            projects::answer_invite(db, *resolved).map(|member| match member {
                Some(member) => CommandResponse::Member(Box::new(member)),
                None => CommandResponse::Unit,
            })
        }
        MetadataCommand::PutProjectCredential(credential, max_access_keys) => {
            projects::put_project_credential(db, *credential, max_access_keys)
                .map(|_| CommandResponse::Unit)
        }
        MetadataCommand::SetCredentialEnabled(access_key, enabled) => {
            projects::set_credential_enabled(db, &access_key, enabled)
                .map(|_| CommandResponse::Unit)
        }
        MetadataCommand::TouchCredential(resolved) => {
            projects::touch_credential(db, *resolved).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::DeleteSession(access_key) => {
            projects::delete_session(db, &access_key).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::PayForProject(pay) => billing_apply::pay_for_project(db, *pay)
            .map(|paid| CommandResponse::Paid(Box::new(paid))),
        MetadataCommand::SetCancel(cancel) => billing_apply::set_cancel(db, *cancel)
            .map(|sub| CommandResponse::Subscription(Box::new(sub))),
        MetadataCommand::AdvanceSubscription(advance) => billing_apply::advance(db, *advance)
            .map(|step| CommandResponse::Advanced(Box::new(step))),
        MetadataCommand::CreateGiftCode(code) => billing_apply::create_gift_code(db, *code)
            .map(|gift| CommandResponse::GiftCode(Box::new(gift))),
        MetadataCommand::SetGiftCodeDisabled(id, disabled) => {
            billing_apply::set_gift_code_disabled(db, id, disabled)
                .map(|gift| CommandResponse::GiftCode(Box::new(gift)))
        }
        MetadataCommand::RedeemGiftCode(redeem) => {
            billing_apply::redeem(db, *redeem).map(|done| CommandResponse::Redeemed(Box::new(done)))
        }
        MetadataCommand::CreateCheckout(new) => billing_apply::create_checkout(db, *new)
            .map(|checkout| CommandResponse::Checkout(Box::new(checkout))),
        MetadataCommand::CompleteCheckout(done) => billing_apply::complete_checkout(db, *done)
            .map(|checkout| CommandResponse::Checkout(Box::new(checkout))),
        MetadataCommand::RebuildDerivedIndexes => {
            billing_apply::rebuild_derived_indexes(db).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::BootstrapCluster(id) => {
            bootstrap_cluster(db, id).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::RegisterNode(cmd) => {
            register_node(db, cmd).map(|n| CommandResponse::NodeInfo(Box::new(n)))
        }
        MetadataCommand::UpdateNodeState(node_id, state) => {
            update_node_state(db, node_id, state).map(|_| CommandResponse::Unit)
        }
    }
}

pub(crate) fn create_bucket(
    db: &Database,
    resolved: ResolvedCreateBucket,
) -> Result<Bucket, MetaError> {
    let ResolvedCreateBucket {
        cmd,
        bucket_id,
        created_at,
    } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    billing_apply::require_writable(&write_txn, cmd.owner_id)?;
    billing_apply::index_bucket(&write_txn, bucket_id, Some(cmd.owner_id))?;
    let bucket = {
        let mut table = write_txn.open_table(BUCKETS).map_err(db_err)?;
        if table.get(cmd.name.as_str()).map_err(db_err)?.is_some() {
            return Err(MetaError::BucketAlreadyExists(
                cmd.name.as_str().to_string(),
            ));
        }
        let bucket = Bucket {
            bucket_id,
            name: cmd.name.clone(),
            owner_id: cmd.owner_id,
            created_at,
            region: cmd.region,
            versioning_state: VersioningState::Disabled,
            quota_bytes: None,
        };
        let bytes = serde_json::to_vec(&bucket)?;
        table
            .insert(cmd.name.as_str(), bytes.as_slice())
            .map_err(db_err)?;
        bucket
    };
    write_txn.commit().map_err(db_err)?;
    Ok(bucket)
}

pub(crate) fn delete_bucket(db: &Database, name: &BucketName) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut buckets = write_txn.open_table(BUCKETS).map_err(db_err)?;
        let bucket_id = {
            let guard = buckets
                .get(name.as_str())
                .map_err(db_err)?
                .ok_or_else(|| MetaError::NoSuchBucket(name.as_str().to_string()))?;
            let bucket: Bucket = serde_json::from_slice(guard.value())?;
            bucket.bucket_id
        };

        {
            let objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
            let prefix = object_row_prefix(bucket_id);
            let mut range = objects.range(prefix.as_str()..).map_err(db_err)?;
            if let Some(entry) = range.next() {
                let (key_guard, _) = entry.map_err(db_err)?;
                if key_guard.value().starts_with(&prefix) {
                    return Err(MetaError::BucketNotEmpty(name.as_str().to_string()));
                }
            }
        }

        buckets.remove(name.as_str()).map_err(db_err)?;
        drop(buckets);
        if let Some(owner) = billing_apply::owner_of_bucket(&write_txn, bucket_id)? {
            billing_apply::require_writable(&write_txn, owner)?;
        }
        billing_apply::index_bucket(&write_txn, bucket_id, None)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn get_bucket(db: &Database, name: &BucketName) -> Result<Option<Bucket>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(BUCKETS).map_err(db_err)?;
    match table.get(name.as_str()).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn list_buckets(db: &Database, owner: OwnerId) -> Result<Vec<Bucket>, MetaError> {
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
}

pub(crate) fn list_all_buckets(db: &Database) -> Result<Vec<Bucket>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(BUCKETS).map_err(db_err)?;
    let mut result = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        result.push(serde_json::from_slice::<Bucket>(value.value())?);
    }
    result.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    Ok(result)
}

pub(crate) fn storage_stats(db: &Database) -> Result<StorageStats, MetaError> {
    fn count_shards(stats: &mut StorageStats, part: &PartManifest) {
        for shard in part.stripes.iter().flat_map(|stripe| &stripe.shards) {
            *stats.shards_by_node.entry(shard.node_id).or_default() += 1;
        }
    }

    let read_txn = db.begin_read().map_err(db_err)?;
    let mut stats = StorageStats::default();

    let objects = read_txn.open_table(OBJECTS).map_err(db_err)?;
    for entry in objects.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        let manifest: ObjectManifest = serde_json::from_slice(value.value())?;
        let usage = stats.buckets.entry(manifest.bucket_id).or_default();
        usage.object_count += 1;
        usage.total_bytes += manifest.total_size();
        for part in &manifest.parts {
            count_shards(&mut stats, part);
        }
    }

    let uploads = read_txn.open_table(MULTIPART).map_err(db_err)?;
    for entry in uploads.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        let upload: MultipartUploadState = serde_json::from_slice(value.value())?;
        stats.multipart_uploads += 1;
        for part in upload.parts.values() {
            count_shards(&mut stats, part);
        }
    }

    Ok(stats)
}

pub(crate) fn commit_manifest(
    db: &Database,
    manifest: ObjectManifest,
) -> Result<ObjectManifest, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let row_key = object_row_key(manifest.bucket_id, &manifest.key);
    let replaced: Option<ObjectManifest> = {
        let objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
        projects::read_row(&objects, &row_key)?
    };
    // Checked here, in the step that stores it, so uploads finishing at the same moment
    // can't together go over the limit. Replacing an object only counts the difference.
    if let Some(owner) = billing_apply::owner_of_bucket(&write_txn, manifest.bucket_id)? {
        let old = replaced.as_ref().map_or(0, |m| m.total_size());
        billing_apply::change_usage(
            &write_txn,
            owner,
            manifest.total_size() as i64 - old as i64,
            i64::from(replaced.is_none()),
            0,
        )?;
    }
    {
        let mut objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
        let bytes = serde_json::to_vec(&manifest)?;
        objects
            .insert(row_key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(manifest)
}

pub(crate) fn get_manifest(
    db: &Database,
    bucket_id: BucketId,
    key: &ObjectKey,
) -> Result<Option<ObjectManifest>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(OBJECTS).map_err(db_err)?;
    let row_key = object_row_key(bucket_id, key);
    match table.get(row_key.as_str()).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn tombstone_object(
    db: &Database,
    bucket_id: BucketId,
    key: &ObjectKey,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let owner = billing_apply::owner_of_bucket(&write_txn, bucket_id)?;
    if let Some(owner) = owner {
        billing_apply::require_writable(&write_txn, owner)?;
    }
    let removed: Option<ObjectManifest> = {
        let mut objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
        let row_key = object_row_key(bucket_id, key);
        match objects.remove(row_key.as_str()).map_err(db_err)? {
            Some(guard) => Some(serde_json::from_slice(guard.value())?),
            None => None,
        }
    };
    if let (Some(owner), Some(removed)) = (owner, removed) {
        billing_apply::release_usage(&write_txn, owner, removed.total_size(), 1, 0)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn list_objects(
    db: &Database,
    query: ListObjectsQuery,
) -> Result<ListObjectsPage, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(OBJECTS).map_err(db_err)?;

    let bucket_prefix = object_row_prefix(query.bucket_id);
    let key_prefix = query.prefix.clone().unwrap_or_default();
    let full_prefix = format!("{bucket_prefix}{key_prefix}");

    let start_after_boundary = query
        .start_after
        .clone()
        .map(|k| format!("{bucket_prefix}{k}"));
    let (range_start, exclusive_boundary): (String, Option<String>) = match &query
        .continuation_token
    {
        Some(token) => {
            let key = crate::cursor::decode(token).ok_or(MetaError::InvalidContinuationToken)?;
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
            break;
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
}

pub(crate) fn begin_multipart(
    db: &Database,
    resolved: ResolvedBeginMultipart,
) -> Result<UploadId, MetaError> {
    let ResolvedBeginMultipart {
        cmd,
        upload_id,
        initiated_at,
    } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    if let Some(owner) = billing_apply::owner_of_bucket(&write_txn, cmd.bucket_id)? {
        billing_apply::require_writable(&write_txn, owner)?;
    }
    {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        let state = MultipartUploadState {
            upload_id,
            bucket_id: cmd.bucket_id,
            key: cmd.key,
            content_type: cmd.content_type,
            user_metadata: cmd.user_metadata,
            initiated_at,
            parts: Default::default(),
        };
        let bytes = serde_json::to_vec(&state)?;
        table
            .insert(upload_id.to_string().as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(upload_id)
}

pub(crate) fn get_upload(
    db: &Database,
    upload_id: UploadId,
) -> Result<Option<MultipartUploadState>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(MULTIPART).map_err(db_err)?;
    match table.get(upload_id.to_string().as_str()).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn record_part(
    db: &Database,
    upload_id: UploadId,
    part: PartManifest,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let key = upload_id.to_string();
    let mut state: MultipartUploadState = {
        let table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        projects::read_row(&table, &key)?
            .ok_or_else(|| MetaError::NoSuchUpload(upload_id.to_string()))?
    };
    // A part counts as soon as it's stored, so uploads in progress can't together go over
    // the limit. Sending a part again only counts the difference.
    if let Some(owner) = billing_apply::owner_of_bucket(&write_txn, state.bucket_id)? {
        let old = state.parts.get(&part.part_number).map_or(0, |p| p.size);
        billing_apply::change_usage(&write_txn, owner, 0, 0, part.size as i64 - old as i64)?;
    }
    {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        state.parts.insert(part.part_number, part);
        let bytes = serde_json::to_vec(&state)?;
        table
            .insert(key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn list_parts(
    db: &Database,
    upload_id: UploadId,
) -> Result<Vec<PartSummary>, MetaError> {
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
}

pub(crate) fn complete_multipart(
    db: &Database,
    resolved: ResolvedCompleteMultipart,
) -> Result<ObjectManifest, MetaError> {
    let ResolvedCompleteMultipart {
        cmd,
        object_id,
        version_id,
        created_at,
    } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    let manifest = {
        let mut multipart = write_txn.open_table(MULTIPART).map_err(db_err)?;
        let key = cmd.upload_id.to_string();
        let state: MultipartUploadState = {
            let guard = multipart
                .get(key.as_str())
                .map_err(db_err)?
                .ok_or_else(|| MetaError::NoSuchUpload(cmd.upload_id.to_string()))?;
            serde_json::from_slice(guard.value())?
        };

        let mut parts = Vec::with_capacity(cmd.requested_parts.len());
        let mut last_part_number: Option<u32> = None;
        for (part_number, etag_hex) in &cmd.requested_parts {
            if let Some(last) = last_part_number
                && *part_number <= last
            {
                return Err(MetaError::InvalidPartOrder);
            }
            last_part_number = Some(*part_number);

            let recorded = state.parts.get(part_number).ok_or(MetaError::InvalidPart)?;
            if hex(&recorded.etag_md5) != *etag_hex {
                return Err(MetaError::InvalidPart);
            }
            parts.push(recorded.clone());
        }
        if parts.is_empty() {
            return Err(MetaError::InvalidPart);
        }

        let mut offset = 0u64;
        for part in &mut parts {
            part.offset = offset;
            offset += part.size;
        }
        let total_size = offset;

        let mut combined_md5_input = Vec::with_capacity(parts.len() * 16);
        for part in &parts {
            combined_md5_input.extend_from_slice(&part.etag_md5);
        }
        let combined_digest: [u8; 16] = Md5::digest(&combined_md5_input).into();
        let etag = ETag::from_multipart_digests(combined_digest, parts.len());

        let mut shard_checksums = Vec::new();
        for part in &parts {
            for stripe in &part.stripes {
                for shard in &stripe.shards {
                    shard_checksums.extend_from_slice(&shard.checksum);
                }
            }
        }
        let sha256: [u8; 32] = {
            use sha2::Digest as _;
            Sha256::digest(&shard_checksums).into()
        };

        let manifest = ObjectManifest {
            object_id,
            bucket_id: state.bucket_id,
            key: state.key.clone(),
            version_id,
            size: total_size,
            etag,
            sha256,
            content_type: cmd.content_type,
            user_metadata: state.user_metadata.clone(),
            created_at,
            delete_marker: false,
            parts,
        };

        multipart.remove(key.as_str()).map_err(db_err)?;
        (manifest, state.parts.values().map(|p| p.size).sum::<u64>())
    };
    let (manifest, recorded_bytes) = manifest;

    // Every recorded part was counted when it arrived, and the object can only be made of
    // them, so finishing never adds usage: it turns parts into an object and frees the
    // parts that weren't used.
    {
        let row_key = object_row_key(manifest.bucket_id, &manifest.key);
        let replaced: Option<ObjectManifest> = {
            let objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
            projects::read_row(&objects, &row_key)?
        };
        if let Some(owner) = billing_apply::owner_of_bucket(&write_txn, manifest.bucket_id)? {
            billing_apply::require_writable(&write_txn, owner)?;
            let old = replaced.as_ref().map_or(0, |m| m.total_size());
            billing_apply::release_usage(
                &write_txn,
                owner,
                old,
                u64::from(replaced.is_some()),
                recorded_bytes,
            )?;
            let current = billing_apply::usage(&write_txn, owner)?;
            let mut table = write_txn
                .open_table(billing_apply::OWNER_USAGE)
                .map_err(db_err)?;
            projects::write_row(
                &mut table,
                &owner.to_string(),
                &crate::billing::OwnerUsage {
                    bytes: current.bytes + manifest.total_size(),
                    objects: current.objects + 1,
                    pending_bytes: current.pending_bytes,
                },
            )?;
        }
    }

    {
        let mut objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
        let row_key = object_row_key(manifest.bucket_id, &manifest.key);
        let bytes = serde_json::to_vec(&manifest)?;
        objects
            .insert(row_key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }

    write_txn.commit().map_err(db_err)?;
    Ok(manifest)
}

pub(crate) fn abort_multipart(db: &Database, upload_id: UploadId) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let removed: Option<MultipartUploadState> = {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        match table
            .remove(upload_id.to_string().as_str())
            .map_err(db_err)?
        {
            Some(guard) => Some(serde_json::from_slice(guard.value())?),
            None => None,
        }
    };
    if let Some(upload) = removed
        && let Some(owner) = billing_apply::owner_of_bucket(&write_txn, upload.bucket_id)?
    {
        let bytes = upload.parts.values().map(|p| p.size).sum();
        billing_apply::release_usage(&write_txn, owner, 0, 0, bytes)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn put_credential(db: &Database, cred: Credential) -> Result<(), MetaError> {
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
}

pub(crate) fn get_credential(
    db: &Database,
    access_key: &str,
) -> Result<Option<Credential>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(CREDENTIALS).map_err(db_err)?;
    match table.get(access_key).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn list_credentials(db: &Database) -> Result<Vec<Credential>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(CREDENTIALS).map_err(db_err)?;
    let mut result = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        result.push(serde_json::from_slice(value.value())?);
    }
    Ok(result)
}

pub(crate) fn delete_credential(db: &Database, access_key: &str) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        table.remove(access_key).map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn record_user_sign_in(
    db: &Database,
    resolved: ResolvedUserSignIn,
) -> Result<User, MetaError> {
    let ResolvedUserSignIn {
        sign_in,
        user_id,
        at,
    } = resolved;
    let subject_key = format!("{}\0{}", sign_in.issuer, sign_in.subject);

    let write_txn = db.begin_write().map_err(db_err)?;
    let user = {
        let mut subjects = write_txn.open_table(USER_SUBJECTS).map_err(db_err)?;
        let mut users = write_txn.open_table(USERS).map_err(db_err)?;

        let existing: Option<UserId> = match subjects.get(subject_key.as_str()).map_err(db_err)? {
            Some(guard) => Some(serde_json::from_slice(guard.value())?),
            None => None,
        };
        let user = match existing {
            Some(existing) => {
                let stored = users
                    .get(existing.to_string().as_str())
                    .map_err(db_err)?
                    .ok_or_else(|| MetaError::NoSuchUser(existing.to_string()))?;
                let mut user: User = serde_json::from_slice(stored.value())?;
                drop(stored);
                user.email = sign_in.email;
                user.email_verified = sign_in.email_verified;
                user.name = sign_in.name;
                user.last_sign_in_at = at;
                user
            }
            None => {
                let id_bytes = serde_json::to_vec(&user_id)?;
                subjects
                    .insert(subject_key.as_str(), id_bytes.as_slice())
                    .map_err(db_err)?;
                User {
                    user_id,
                    issuer: sign_in.issuer,
                    subject: sign_in.subject,
                    email: sign_in.email,
                    email_verified: sign_in.email_verified,
                    name: sign_in.name,
                    enabled: true,
                    created_at: at,
                    last_sign_in_at: at,
                }
            }
        };
        let bytes = serde_json::to_vec(&user)?;
        users
            .insert(user.user_id.to_string().as_str(), bytes.as_slice())
            .map_err(db_err)?;
        user
    };
    write_txn.commit().map_err(db_err)?;
    Ok(user)
}

pub(crate) fn get_user(db: &Database, user_id: UserId) -> Result<Option<User>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(USERS).map_err(db_err)?;
    match table.get(user_id.to_string().as_str()).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn list_users(db: &Database) -> Result<Vec<User>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(USERS).map_err(db_err)?;
    let mut result = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        result.push(serde_json::from_slice(value.value())?);
    }
    Ok(result)
}

pub(crate) fn set_user_access(
    db: &Database,
    user_id: UserId,
    access: UserAccess,
) -> Result<User, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let user = {
        let mut table = write_txn.open_table(USERS).map_err(db_err)?;
        let key = user_id.to_string();
        let mut user: User = match table.get(key.as_str()).map_err(db_err)? {
            Some(guard) => serde_json::from_slice(guard.value())?,
            None => return Err(MetaError::NoSuchUser(key)),
        };
        user.enabled = access.enabled;
        let bytes = serde_json::to_vec(&user)?;
        table
            .insert(key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
        user
    };
    if !access.enabled {
        let mut credentials = write_txn.open_table(CREDENTIALS).map_err(db_err)?;
        projects::remove_credentials(&mut credentials, |c| {
            c.kind == CredentialKind::ProjectSession && c.user_id == Some(user_id)
        })?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(user)
}

pub(crate) fn bootstrap_cluster(db: &Database, cluster_id: ClusterId) -> Result<(), MetaError> {
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
}

pub(crate) fn get_cluster_id(db: &Database) -> Result<Option<ClusterId>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(CLUSTER).map_err(db_err)?;
    match table.get(CLUSTER_ID_KEY).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
}

/// The state a (re)registering node ends up in. A restart must not undo an operator's
/// decision, so a draining node stays draining and a removed node can't come back.
pub(crate) fn registration_state(
    node_id: NodeId,
    existing: Option<NodeState>,
) -> Result<NodeState, MetaError> {
    match existing {
        Some(NodeState::Removed) => Err(MetaError::NodeRemoved(node_id.to_string())),
        Some(NodeState::Draining) => Ok(NodeState::Draining),
        _ => Ok(NodeState::Joining),
    }
}

/// Node states an operator controls can't be overwritten by automation: the heartbeat
/// loop reads a node's state, waits on a network call, then writes the outcome, so a
/// drain landing in between would otherwise be silently reverted to `Healthy`. Leaving
/// `Draining` means `Joining` (an explicit resume, which heartbeats then promote) or
/// `Removed`, the only way to reach `Removed`, and which is final.
pub(crate) fn check_node_transition(
    node_id: NodeId,
    from: NodeState,
    to: NodeState,
) -> Result<(), MetaError> {
    let allowed = from == to
        || match (from, to) {
            (NodeState::Removed, _) => false,
            (NodeState::Draining, target) => {
                matches!(target, NodeState::Joining | NodeState::Removed)
            }
            (_, NodeState::Removed) => false,
            _ => true,
        };
    if allowed {
        Ok(())
    } else {
        Err(MetaError::InvalidNodeTransition {
            node: node_id.to_string(),
            from: format!("{from:?}"),
            to: format!("{to:?}"),
        })
    }
}

pub(crate) fn register_node(db: &Database, cmd: RegisterNode) -> Result<NodeInfo, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let info = {
        let mut table = write_txn.open_table(NODES).map_err(db_err)?;
        let key = cmd.node_id.to_string();
        let existing: Option<NodeInfo> = match table.get(key.as_str()).map_err(db_err)? {
            Some(guard) => Some(serde_json::from_slice(guard.value())?),
            None => None,
        };
        let state = registration_state(cmd.node_id, existing.as_ref().map(|n| n.state))?;
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
}

pub(crate) fn update_node_state(
    db: &Database,
    node_id: NodeId,
    state: NodeState,
) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(NODES).map_err(db_err)?;
        let key = node_id.to_string();
        let mut info: NodeInfo = match table.get(key.as_str()).map_err(db_err)? {
            Some(guard) => serde_json::from_slice(guard.value())?,
            None => return Err(MetaError::NoSuchNode(key)),
        };
        check_node_transition(node_id, info.state, state)?;
        info.state = state;
        info.last_seen = OffsetDateTime::now_utc();
        let bytes = serde_json::to_vec(&info)?;
        table
            .insert(key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn list_nodes(db: &Database) -> Result<Vec<NodeInfo>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(NODES).map_err(db_err)?;
    let mut result = Vec::new();
    for entry in table.iter().map_err(db_err)? {
        let (_key, value) = entry.map_err(db_err)?;
        result.push(serde_json::from_slice(value.value())?);
    }
    result.sort_by_key(|n: &NodeInfo| n.node_id.to_string());
    Ok(result)
}
