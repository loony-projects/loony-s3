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

use md5::{Digest as _, Md5};
use redb::{Database, ReadableTable, TableDefinition};
use s3_core::{
    Bucket, BucketId, BucketName, ClusterId, ETag, NodeId, NodeInfo, NodeState, ObjectKey,
    ObjectManifest, OwnerId, UploadId, VersioningState,
};
use sha2::Sha256;
use time::OffsetDateTime;

use crate::commands::{
    Credential, ListObjectsPage, ListObjectsQuery, MultipartUploadState, ObjectSummary, PartSummary,
    RegisterNode,
};
use crate::error::MetaError;
use crate::raft::types::{
    CommandResponse, MetadataCommand, ResolvedBeginMultipart, ResolvedCompleteMultipart,
    ResolvedCreateBucket,
};
use s3_core::PartManifest;

pub(crate) const BUCKETS: TableDefinition<&str, &[u8]> = TableDefinition::new("buckets");
pub(crate) const OBJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("objects");
pub(crate) const MULTIPART: TableDefinition<&str, &[u8]> = TableDefinition::new("multipart_uploads");
pub(crate) const CREDENTIALS: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");
pub(crate) const NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
pub(crate) const CLUSTER: TableDefinition<&str, &[u8]> = TableDefinition::new("cluster");
pub(crate) const CLUSTER_ID_KEY: &str = "cluster_id";

/// Every business-data table the state machine owns — used to open/create them upfront
/// and to enumerate them wholesale for snapshotting (`snapshot.rs`).
pub(crate) const ALL_TABLES: &[TableDefinition<'static, &'static str, &'static [u8]>] =
    &[BUCKETS, OBJECTS, MULTIPART, CREDENTIALS, NODES, CLUSTER];

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
        MetadataCommand::CreateBucket(cmd) => create_bucket(db, cmd).map(|b| CommandResponse::Bucket(Box::new(b))),
        MetadataCommand::DeleteBucket(name) => delete_bucket(db, &name).map(|_| CommandResponse::Unit),
        MetadataCommand::CommitManifest(manifest) => {
            commit_manifest(db, *manifest).map(|m| CommandResponse::Manifest(Box::new(m)))
        }
        MetadataCommand::TombstoneObject(bucket_id, key) => {
            tombstone_object(db, bucket_id, &key).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::BeginMultipart(cmd) => begin_multipart(db, cmd).map(CommandResponse::UploadId),
        MetadataCommand::RecordPart(upload_id, part) => {
            record_part(db, upload_id, *part).map(|_| CommandResponse::Unit)
        }
        MetadataCommand::CompleteMultipart(cmd) => {
            complete_multipart(db, *cmd).map(|m| CommandResponse::Manifest(Box::new(m)))
        }
        MetadataCommand::AbortMultipart(upload_id) => abort_multipart(db, upload_id).map(|_| CommandResponse::Unit),
        MetadataCommand::PutCredential(cred) => put_credential(db, *cred).map(|_| CommandResponse::Unit),
        MetadataCommand::BootstrapCluster(id) => bootstrap_cluster(db, id).map(|_| CommandResponse::Unit),
        MetadataCommand::RegisterNode(cmd) => register_node(db, cmd).map(|n| CommandResponse::NodeInfo(Box::new(n))),
        MetadataCommand::UpdateNodeState(node_id, state) => {
            update_node_state(db, node_id, state).map(|_| CommandResponse::Unit)
        }
    }
}

pub(crate) fn create_bucket(db: &Database, resolved: ResolvedCreateBucket) -> Result<Bucket, MetaError> {
    let ResolvedCreateBucket { cmd, bucket_id, created_at } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    let bucket = {
        let mut table = write_txn.open_table(BUCKETS).map_err(db_err)?;
        if table.get(cmd.name.as_str()).map_err(db_err)?.is_some() {
            return Err(MetaError::BucketAlreadyExists(cmd.name.as_str().to_string()));
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

pub(crate) fn commit_manifest(db: &Database, manifest: ObjectManifest) -> Result<ObjectManifest, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
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

pub(crate) fn tombstone_object(db: &Database, bucket_id: BucketId, key: &ObjectKey) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
        let row_key = object_row_key(bucket_id, key);
        objects.remove(row_key.as_str()).map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn list_objects(db: &Database, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(OBJECTS).map_err(db_err)?;

    let bucket_prefix = object_row_prefix(query.bucket_id);
    let key_prefix = query.prefix.clone().unwrap_or_default();
    let full_prefix = format!("{bucket_prefix}{key_prefix}");

    let start_after_boundary = query
        .start_after
        .clone()
        .map(|k| format!("{bucket_prefix}{k}"));
    let (range_start, exclusive_boundary): (String, Option<String>) = match &query.continuation_token {
        Some(token) => (token.clone(), None),
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
                        next_token = Some(row_key);
                        break;
                    }
                    common_prefixes.push(common);
                }
                continue;
            }
        }

        if objects.len() + common_prefixes.len() >= max_keys {
            is_truncated = true;
            next_token = Some(row_key);
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

pub(crate) fn begin_multipart(db: &Database, resolved: ResolvedBeginMultipart) -> Result<UploadId, MetaError> {
    let ResolvedBeginMultipart { cmd, upload_id, initiated_at } = resolved;
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        let state = MultipartUploadState {
            upload_id,
            bucket_id: cmd.bucket_id,
            key: cmd.key,
            content_type: cmd.content_type,
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

pub(crate) fn record_part(db: &Database, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        let key = upload_id.to_string();
        let mut state: MultipartUploadState = {
            let guard = table
                .get(key.as_str())
                .map_err(db_err)?
                .ok_or_else(|| MetaError::NoSuchUpload(upload_id.to_string()))?;
            serde_json::from_slice(guard.value())?
        };
        state.parts.insert(part.part_number, part);
        let bytes = serde_json::to_vec(&state)?;
        table
            .insert(key.as_str(), bytes.as_slice())
            .map_err(db_err)?;
    }
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

pub(crate) fn list_parts(db: &Database, upload_id: UploadId) -> Result<Vec<PartSummary>, MetaError> {
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

pub(crate) fn complete_multipart(db: &Database, resolved: ResolvedCompleteMultipart) -> Result<ObjectManifest, MetaError> {
    let ResolvedCompleteMultipart { cmd, object_id, version_id, created_at } = resolved;
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
            user_metadata: Default::default(),
            created_at,
            delete_marker: false,
            parts,
        };

        multipart.remove(key.as_str()).map_err(db_err)?;
        manifest
    };

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
    {
        let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
        table
            .remove(upload_id.to_string().as_str())
            .map_err(db_err)?;
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

pub(crate) fn get_credential(db: &Database, access_key: &str) -> Result<Option<Credential>, MetaError> {
    let read_txn = db.begin_read().map_err(db_err)?;
    let table = read_txn.open_table(CREDENTIALS).map_err(db_err)?;
    match table.get(access_key).map_err(db_err)? {
        Some(guard) => Ok(Some(serde_json::from_slice(guard.value())?)),
        None => Ok(None),
    }
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
                table.insert(CLUSTER_ID_KEY, bytes.as_slice()).map_err(db_err)?;
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

pub(crate) fn register_node(db: &Database, cmd: RegisterNode) -> Result<NodeInfo, MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    let info = {
        let mut table = write_txn.open_table(NODES).map_err(db_err)?;
        let key = cmd.node_id.to_string();
        let generation = match table.get(key.as_str()).map_err(db_err)? {
            Some(guard) => {
                let existing: NodeInfo = serde_json::from_slice(guard.value())?;
                existing.generation + 1
            }
            None => 0,
        };
        let info = NodeInfo {
            node_id: cmd.node_id,
            advertised_address: cmd.advertised_address,
            state: NodeState::Joining,
            generation,
            last_seen: OffsetDateTime::now_utc(),
            failure_domain: cmd.failure_domain,
        };
        let bytes = serde_json::to_vec(&info)?;
        table.insert(key.as_str(), bytes.as_slice()).map_err(db_err)?;
        info
    };
    write_txn.commit().map_err(db_err)?;
    Ok(info)
}

pub(crate) fn update_node_state(db: &Database, node_id: NodeId, state: NodeState) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    {
        let mut table = write_txn.open_table(NODES).map_err(db_err)?;
        let key = node_id.to_string();
        let mut info: NodeInfo = match table.get(key.as_str()).map_err(db_err)? {
            Some(guard) => serde_json::from_slice(guard.value())?,
            None => return Err(MetaError::NoSuchNode(key)),
        };
        info.state = state;
        info.last_seen = OffsetDateTime::now_utc();
        let bytes = serde_json::to_vec(&info)?;
        table.insert(key.as_str(), bytes.as_slice()).map_err(db_err)?;
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
