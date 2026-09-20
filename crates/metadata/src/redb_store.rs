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
use md5::{Digest as _, Md5};
use redb::{Database, ReadableTable, TableDefinition};
use s3_core::{
    Bucket, BucketId, BucketName, ClusterId, ETag, NodeId, NodeInfo, NodeState, ObjectId,
    ObjectKey, ObjectManifest, OwnerId, PartManifest, UploadId, VersionId, VersioningState,
};
use sha2::Sha256;
use time::OffsetDateTime;

use crate::commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, Credential, ListObjectsPage, ListObjectsQuery,
    MultipartUploadState, ObjectSummary, PartSummary, RegisterNode,
};
use crate::error::MetaError;
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            let bucket = {
                let mut table = write_txn.open_table(BUCKETS).map_err(db_err)?;
                if table.get(cmd.name.as_str()).map_err(db_err)?.is_some() {
                    return Err(MetaError::BucketAlreadyExists(
                        cmd.name.as_str().to_string(),
                    ));
                }
                let bucket = Bucket {
                    bucket_id: BucketId::new(),
                    name: cmd.name.clone(),
                    owner_id: cmd.owner_id,
                    created_at: OffsetDateTime::now_utc(),
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
        })
        .await
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<(), MetaError> {
        let name = name.clone();
        self.blocking(move |db| {
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
        })
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
        self.blocking(move |db| {
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
        })
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
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            {
                let mut objects = write_txn.open_table(OBJECTS).map_err(db_err)?;
                let row_key = object_row_key(bucket_id, &key);
                objects.remove(row_key.as_str()).map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn list_objects(&self, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError> {
        self.blocking(move |db| {
            let read_txn = db.begin_read().map_err(db_err)?;
            let table = read_txn.open_table(OBJECTS).map_err(db_err)?;

            let bucket_prefix = object_row_prefix(query.bucket_id);
            let key_prefix = query.prefix.clone().unwrap_or_default();
            let full_prefix = format!("{bucket_prefix}{key_prefix}");

            // `continuation_token` is our own opaque cursor: it's always exactly the
            // row key of the first not-yet-returned item, so resuming from it means
            // ranging from it inclusively with no further skipping. `start_after` is a
            // client-supplied literal key with AWS's exclusive-lower-bound semantics,
            // so it ranges from the same point but the boundary row itself is skipped.
            let start_after_boundary = query
                .start_after
                .clone()
                .map(|k| format!("{bucket_prefix}{k}"));
            let (range_start, exclusive_boundary): (String, Option<String>) =
                match &query.continuation_token {
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
        })
        .await
    }

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            let upload_id = UploadId::new();
            {
                let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
                let state = MultipartUploadState {
                    upload_id,
                    bucket_id: cmd.bucket_id,
                    key: cmd.key,
                    content_type: cmd.content_type,
                    initiated_at: OffsetDateTime::now_utc(),
                    parts: Default::default(),
                };
                let bytes = serde_json::to_vec(&state)?;
                table
                    .insert(upload_id.to_string().as_str(), bytes.as_slice())
                    .map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(upload_id)
        })
        .await
    }

    async fn record_part(&self, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError> {
        self.blocking(move |db| {
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
        })
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
        self.blocking(move |db| {
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

                // Recompute cumulative offsets: parts are recorded independently (and
                // may be re-uploaded out of order before Complete), so their stored
                // `offset` isn't trustworthy until the final ordering is fixed here.
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

                // A multipart object's whole-body SHA-256 would require re-reading
                // every part's bytes, exactly what multipart upload exists to avoid
                // (prompt §27/§28). We instead hash the concatenation of each shard's
                // already-computed checksum: deterministic and still catches manifest
                // corruption, but it is not `SHA256(object bytes)` — documented
                // compatibility difference, architecture.md §29.
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
                    object_id: ObjectId::new(),
                    bucket_id: state.bucket_id,
                    key: state.key.clone(),
                    version_id: VersionId::new(),
                    size: total_size,
                    etag,
                    sha256,
                    content_type: cmd.content_type,
                    user_metadata: Default::default(),
                    created_at: OffsetDateTime::now_utc(),
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
        })
        .await
    }

    async fn abort_multipart(&self, upload_id: UploadId) -> Result<(), MetaError> {
        self.blocking(move |db| {
            let write_txn = db.begin_write().map_err(db_err)?;
            {
                let mut table = write_txn.open_table(MULTIPART).map_err(db_err)?;
                table
                    .remove(upload_id.to_string().as_str())
                    .map_err(db_err)?;
            }
            write_txn.commit().map_err(db_err)?;
            Ok(())
        })
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
    use s3_core::{DurabilityPolicy, NodeId, ShardLocation, Stripe, VolumeId};

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
            shard_id: s3_core::ShardId::new(),
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

        let id = s3_core::ClusterId::parse("cluster-one").unwrap();
        store.bootstrap_cluster(id.clone()).await.unwrap();
        assert_eq!(store.get_cluster_id().await.unwrap(), Some(id.clone()));

        // Calling it again with the same id is a no-op, not an error.
        store.bootstrap_cluster(id.clone()).await.unwrap();

        let other = s3_core::ClusterId::parse("cluster-two").unwrap();
        let err = store.bootstrap_cluster(other).await.unwrap_err();
        assert!(matches!(err, MetaError::ClusterIdMismatch { .. }));
        // The mismatch attempt didn't change anything.
        assert_eq!(store.get_cluster_id().await.unwrap(), Some(id));
    }

    #[tokio::test]
    async fn register_node_bumps_generation_on_rejoin() {
        let store = store().await;
        let node_id = s3_core::NodeId::new();

        let first = store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec!["rack:a".into()],
            })
            .await
            .unwrap();
        assert_eq!(first.generation, 0);
        assert_eq!(first.state, s3_core::NodeState::Joining);

        let second = store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec!["rack:a".into()],
            })
            .await
            .unwrap();
        assert_eq!(second.generation, 1);
    }

    #[tokio::test]
    async fn update_node_state_requires_prior_registration() {
        let store = store().await;
        let node_id = s3_core::NodeId::new();

        let err = store
            .update_node_state(node_id, s3_core::NodeState::Healthy)
            .await
            .unwrap_err();
        assert!(matches!(err, MetaError::NoSuchNode(_)));

        store
            .register_node(RegisterNode {
                node_id,
                advertised_address: "node-a:9100".into(),
                failure_domain: vec![],
            })
            .await
            .unwrap();
        store
            .update_node_state(node_id, s3_core::NodeState::Healthy)
            .await
            .unwrap();

        let nodes = store.list_nodes().await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].state, s3_core::NodeState::Healthy);
    }

    #[tokio::test]
    async fn list_nodes_returns_every_registered_node() {
        let store = store().await;
        for i in 0..3 {
            store
                .register_node(RegisterNode {
                    node_id: s3_core::NodeId::new(),
                    advertised_address: format!("node-{i}:9100"),
                    failure_domain: vec![],
                })
                .await
                .unwrap();
        }
        assert_eq!(store.list_nodes().await.unwrap().len(), 3);
    }
}
