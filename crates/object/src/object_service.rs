use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::try_join_all;
use md5::{Digest as _, Md5};
use loony_core::{
    Bucket, BucketName, DurabilityPolicy, ETag, NodeId, NodeState, ObjectId, ObjectKey,
    ObjectManifest, OwnerId, PartManifest, ShardLocation, ShardTarget, Stripe, UploadId, VersionId,
    VolumeId,
};
use loony_erasure::{ErasureCodec, RsErasureCodec};
use loony_metadata::{
    BeginMultipart, CompleteMultipart, ListObjectsPage, ListObjectsQuery, MetaError, MetadataStore,
    PartSummary,
};
use loony_storage::{ShardBytesIn, ShardBytesOut, ShardStore, StorageError};
use sha2::Sha256;
use time::OffsetDateTime;

use crate::error::Ls3Error;
use crate::stripe_reader::StripeReader;

/// Objects smaller than this get `DurabilityPolicy::Replicated` instead of erasure
/// coding — naive erasure coding on tiny objects is dominated by fixed per-stripe/
/// shard overhead (architecture.md §9/§17).
const SMALL_OBJECT_THRESHOLD: usize = 512 * 1024;
/// Logical (pre-encoding) size of one stripe for erasure-coded objects
/// (architecture.md §8).
const STRIPE_SIZE: usize = 8 * 1024 * 1024;

/// The Object Service (architecture.md §1/§5): PutObject/GetObject/HeadObject/
/// DeleteObject/ListObjectsV2, orchestrating [`MetadataStore`], [`ShardStore`], and
/// (from this phase on) [`loony_erasure::ErasureCodec`] for real durability, plus
/// ownership-based authorization (architecture.md §55) on every method.
///
/// **Durability policy** (architecture.md §9): objects under [`SMALL_OBJECT_THRESHOLD`]
/// are replicated (`n` copies, `n = min(3, available volumes)`); objects at or above it
/// are split into [`STRIPE_SIZE`] stripes and erasure-coded independently, streaming —
/// never buffering more than one stripe at a time (architecture.md §81). The threshold
/// decision itself only needs to look at the first `SMALL_OBJECT_THRESHOLD` bytes, so
/// PUT peeks that far, decides, and then either treats what it peeked as the whole
/// object or feeds it as the start of the first stripe.
///
/// **Placement** (architecture.md §10 simplified for a single node, real
/// rendezvous-hashing `PlacementEngine` is Phase 9's job once there's more than one
/// node): shards of a stripe go to distinct local volumes via round-robin, offset by
/// stripe index so consecutive stripes don't all land on the same subset. The erasure
/// scheme itself adapts to how many volumes exist ([`choose_erasure_scheme`]) so a
/// small dev setup degrades gracefully instead of erroring.
pub struct ObjectService {
    metadata: Arc<dyn MetadataStore>,
    shard_store: Arc<dyn ShardStore>,
    node_id: NodeId,
    volumes: Vec<VolumeId>,
}

/// Picks `(data, parity)` for the volumes actually available. `4+2` (the documented
/// default, architecture.md §8) only kicks in once there are at least 6 volumes to
/// spread shards across; smaller setups get a smaller scheme rather than silently
/// co-locating multiple shards of a stripe on the same volume.
fn choose_erasure_scheme(volume_count: usize) -> (usize, usize) {
    match volume_count {
        0 | 1 => (1, 0),
        2 => (1, 1),
        3 => (2, 1),
        4 => (2, 2),
        5 => (3, 2),
        _ => (4, 2),
    }
}

impl ObjectService {
    pub fn new(
        metadata: Arc<dyn MetadataStore>,
        shard_store: Arc<dyn ShardStore>,
        node_id: NodeId,
        volumes: Vec<VolumeId>,
    ) -> Self {
        Self {
            metadata,
            shard_store,
            node_id,
            volumes,
        }
    }

    async fn authorized_bucket(
        &self,
        bucket: &BucketName,
        requesting_owner: OwnerId,
    ) -> Result<Bucket, Ls3Error> {
        let bucket = self
            .metadata
            .get_bucket(bucket)
            .await?
            .ok_or(Ls3Error::NoSuchBucket)?;
        if bucket.owner_id != requesting_owner {
            return Err(Ls3Error::AccessDenied);
        }
        Ok(bucket)
    }

    /// Cluster-wide placement candidates (architecture.md §10): this node's own local
    /// volumes, always included, plus every other `HEALTHY` node's advertised volumes
    /// from the (Raft-replicated, Phase 8) node registry. In standalone mode
    /// `metadata.list_nodes()` naturally returns nothing (no node ever registers with
    /// it), so this degrades to exactly the local-only candidate set pre-Phase-9 code
    /// used -- no special-casing needed for that case.
    async fn placement_candidates(&self) -> Vec<loony_placement::Candidate> {
        let mut candidates: Vec<loony_placement::Candidate> = self
            .volumes
            .iter()
            .map(|&v| loony_placement::Candidate::new(self.node_id, v))
            .collect();

        if let Ok(nodes) = self.metadata.list_nodes().await {
            for node in nodes {
                if node.node_id == self.node_id || node.state != NodeState::Healthy {
                    continue; // self already added above; only HEALTHY peers qualify
                }
                candidates.extend(
                    node.volumes
                        .iter()
                        .map(|&v| loony_placement::Candidate::new(node.node_id, v)),
                );
            }
        }
        candidates
    }

    async fn write_shard(
        &self,
        target: ShardTarget,
        data: Vec<u8>,
    ) -> Result<loony_core::ShardReceipt, StorageError> {
        let stream: ShardBytesIn =
            Box::pin(futures::stream::once(async move { Ok(Bytes::from(data)) }));
        self.shard_store.put_shard(target, stream).await
    }

    /// Writes one stripe's already-encoded shards concurrently, placing each one via
    /// the placement engine (architecture.md §10) rather than always targeting a local
    /// volume, and returns their `ShardLocation`s in shard-index order.
    #[allow(clippy::too_many_arguments)]
    async fn write_stripe_shards(
        &self,
        object_id: ObjectId,
        version_id: VersionId,
        stripe_index: u32,
        shard_bytes: Vec<Vec<u8>>,
    ) -> Result<Vec<ShardLocation>, Ls3Error> {
        let candidates = self.placement_candidates().await;
        let plan = loony_placement::plan_write(
            object_id,
            version_id,
            stripe_index,
            shard_bytes.len(),
            &candidates,
        )
        .map_err(|e| Ls3Error::InvalidArgument(e.to_string()))?;

        let writes = shard_bytes.into_iter().zip(plan).map(|(bytes, placed)| {
            let target = ShardTarget {
                node_id: placed.node_id,
                volume_id: placed.volume_id,
            };
            async move {
                let receipt = self.write_shard(target, bytes).await?;
                Ok::<ShardLocation, StorageError>(ShardLocation {
                    shard_index: placed.shard_index,
                    node_id: placed.node_id,
                    volume_id: placed.volume_id,
                    shard_id: receipt.shard_id,
                    size: receipt.size,
                    checksum: receipt.checksum,
                    generation: 0,
                })
            }
        });
        Ok(try_join_all(writes).await?)
    }

    pub async fn put_object(
        &self,
        bucket: &BucketName,
        key: ObjectKey,
        content_type: String,
        user_metadata: BTreeMap<String, String>,
        body: ShardBytesIn,
        requesting_owner: OwnerId,
    ) -> Result<ObjectManifest, Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;

        // Minted once, here, on the coordinator -- never inside a replicated apply()
        // the way `commit_manifest`'s embedded manifest already assumes (see
        // `loony-metadata`'s `ResolvedCreateBucket` doc comment for why that distinction
        // matters). Also doubles as the placement engine's stripe-key input.
        let object_id = ObjectId::new();
        let version_id = VersionId::new();
        let candidate_count = self.placement_candidates().await.len();

        let (stripes, total_size, sha256_digest, md5_digest) = self
            .encode_body_to_stripes(object_id, version_id, candidate_count, body)
            .await?;

        let manifest = ObjectManifest {
            object_id,
            bucket_id: bucket.bucket_id,
            key: key.clone(),
            version_id,
            size: total_size,
            etag: ETag::from_md5(md5_digest),
            sha256: sha256_digest,
            content_type,
            user_metadata,
            created_at: OffsetDateTime::now_utc(),
            delete_marker: false,
            parts: vec![PartManifest {
                part_number: 1,
                offset: 0,
                size: total_size,
                etag_md5: md5_digest,
                stripes,
            }],
        };

        Ok(self.metadata.commit_manifest(manifest).await?)
    }

    /// Streams `body` into durable, placed shards exactly the way a whole-object
    /// `put_object` does -- small-object replication vs. streaming stripe-by-stripe
    /// erasure coding (architecture.md §8/§9), never buffering more than one stripe at a
    /// time. Shared by `put_object` (as part 1 of 1) and `upload_part` (as one part of a
    /// multipart upload, architecture.md §15) since a part's bytes are durable and
    /// placed exactly the same way a whole small/large object's bytes are -- multipart
    /// only changes how many `PartManifest`s a `commit_manifest` call ends up holding,
    /// never how any single one of them gets its bytes onto disk.
    async fn encode_body_to_stripes(
        &self,
        object_id: ObjectId,
        version_id: VersionId,
        candidate_count: usize,
        mut body: ShardBytesIn,
    ) -> Result<(Vec<Stripe>, u64, [u8; 32], [u8; 16]), Ls3Error> {
        // Peek up to the small-object threshold to decide durability policy without
        // buffering the whole object (architecture.md §9/§81).
        let mut head = Vec::new();
        let mut exhausted = false;
        while head.len() < SMALL_OBJECT_THRESHOLD {
            match body.next().await {
                Some(Ok(chunk)) => head.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(StorageError::Io(e).into()),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }

        let mut sha256 = Sha256::new();
        let mut md5 = Md5::new();
        let mut stripes = Vec::new();
        let mut total_size: u64 = 0;

        if exhausted {
            // Small object: replicate the bytes we already have as-is.
            use sha2::Digest as _;
            sha256.update(&head);
            md5.update(&head);
            total_size = head.len() as u64;

            let n = candidate_count.clamp(1, 3);
            let shard_bytes: Vec<Vec<u8>> = (0..n).map(|_| head.clone()).collect();
            let shards = self
                .write_stripe_shards(object_id, version_id, 0, shard_bytes)
                .await?;
            stripes.push(Stripe {
                stripe_index: 0,
                stripe_offset: 0,
                stripe_len: total_size,
                durability: DurabilityPolicy::Replicated { n: n as u8 },
                shards,
            });
        } else {
            // Large object: erasure-code it, stripe by stripe, starting with what we
            // already peeked and continuing to stream the rest.
            let (data_count, parity_count) = choose_erasure_scheme(candidate_count);
            let codec = RsErasureCodec::new(data_count, parity_count);
            let durability = DurabilityPolicy::Erasure {
                data: data_count as u8,
                parity: parity_count as u8,
            };

            let mut reader = StripeReader::with_leftover(body, head);
            let mut stripe_index: u32 = 0;
            let mut offset: u64 = 0;

            while let Some(stripe_data) = reader
                .next_stripe(STRIPE_SIZE)
                .await
                .map_err(StorageError::Io)?
            {
                use sha2::Digest as _;
                sha256.update(&stripe_data);
                md5.update(&stripe_data);
                let stripe_len = stripe_data.len() as u64;

                let encoded = codec.encode(&stripe_data).map_err(|e| {
                    Ls3Error::InvalidArgument(format!("erasure encoding failed: {e}"))
                })?;
                let shards = self
                    .write_stripe_shards(object_id, version_id, stripe_index, encoded.shards)
                    .await?;

                stripes.push(Stripe {
                    stripe_index,
                    stripe_offset: offset,
                    stripe_len,
                    durability,
                    shards,
                });

                offset += stripe_len;
                total_size += stripe_len;
                stripe_index += 1;
            }

            if stripes.is_empty() {
                // A zero-byte object still needs exactly one (empty) stripe.
                let encoded = codec.encode(&[]).map_err(|e| {
                    Ls3Error::InvalidArgument(format!("erasure encoding failed: {e}"))
                })?;
                let shards = self
                    .write_stripe_shards(object_id, version_id, 0, encoded.shards)
                    .await?;
                stripes.push(Stripe {
                    stripe_index: 0,
                    stripe_offset: 0,
                    stripe_len: 0,
                    durability,
                    shards,
                });
            }
        }

        let sha256_digest: [u8; 32] = {
            use sha2::Digest as _;
            sha256.finalize().into()
        };
        let md5_digest: [u8; 16] = md5.finalize().into();

        Ok((stripes, total_size, sha256_digest, md5_digest))
    }

    /// Starts a multipart upload (architecture.md §15 / prompt §27): allocates
    /// `upload_id` and records `(bucket, key, content_type, user_metadata)` so any API
    /// node can continue it afterward -- the upload's own record lives in the
    /// (Raft-replicated, Phase 8) metadata store, not on whichever node happens to
    /// handle this request.
    pub async fn create_multipart_upload(
        &self,
        bucket: &BucketName,
        key: ObjectKey,
        content_type: String,
        user_metadata: BTreeMap<String, String>,
        requesting_owner: OwnerId,
    ) -> Result<UploadId, Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;
        Ok(self
            .metadata
            .begin_multipart(BeginMultipart {
                bucket_id: bucket.bucket_id,
                key,
                content_type,
                user_metadata,
            })
            .await?)
    }

    /// Confirms `upload_id` both exists and genuinely belongs to `(bucket, key)` under
    /// `requesting_owner`'s bucket -- `upload_id` alone can't be trusted the way a
    /// bucket/key pair already authorized via [`Self::authorized_bucket`] can, since
    /// nothing about its value reveals which bucket it was created under.
    async fn authorized_upload(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        requesting_owner: OwnerId,
    ) -> Result<loony_metadata::MultipartUploadState, Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;
        let state = self
            .metadata
            .get_upload(upload_id)
            .await?
            .ok_or(Ls3Error::NoSuchUpload)?;
        if state.bucket_id != bucket.bucket_id || &state.key != key {
            return Err(Ls3Error::NoSuchUpload);
        }
        Ok(state)
    }

    /// Uploads one part (architecture.md §15): the same durable, placed
    /// encode-and-write pipeline as a whole-object PUT, recorded against `upload_id` via
    /// `RecordPart` once written. Re-uploading the same `part_number` before Complete
    /// overwrites what was recorded, matching LS3 semantics.
    pub async fn upload_part(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        part_number: u32,
        body: ShardBytesIn,
        requesting_owner: OwnerId,
    ) -> Result<ETag, Ls3Error> {
        self.authorized_upload(bucket, key, upload_id, requesting_owner)
            .await?;
        if !(1..=10_000).contains(&part_number) {
            return Err(Ls3Error::InvalidArgument(
                "part number must be between 1 and 10000".into(),
            ));
        }

        // Placement identity for this part's shards -- purely a hashing seed for
        // spreading load across candidates (architecture.md §10), never persisted
        // anywhere, so (unlike `commit_manifest`'s object_id/version_id) freshness is
        // all that's needed here, not cross-replica agreement.
        let object_id = ObjectId::new();
        let version_id = VersionId::new();
        let candidate_count = self.placement_candidates().await.len();

        let (stripes, size, _sha256, md5_digest) = self
            .encode_body_to_stripes(object_id, version_id, candidate_count, body)
            .await?;
        let etag = ETag::from_md5(md5_digest);

        self.metadata
            .record_part(
                upload_id,
                PartManifest {
                    part_number,
                    // Recomputed from the final, validated part ordering at
                    // CompleteMultipartUpload time (parts can be re-uploaded, possibly
                    // out of order, until then) -- not trustworthy here.
                    offset: 0,
                    size,
                    etag_md5: md5_digest,
                    stripes,
                },
            )
            .await
            .map_err(map_multipart_meta_err)?;
        Ok(etag)
    }

    /// Pure metadata read (architecture.md §15): every part recorded so far for
    /// `upload_id`, regardless of whether it'll end up included in the eventual
    /// `CompleteMultipartUpload`.
    pub async fn list_parts(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        requesting_owner: OwnerId,
    ) -> Result<Vec<PartSummary>, Ls3Error> {
        self.authorized_upload(bucket, key, upload_id, requesting_owner)
            .await?;
        self.metadata
            .list_parts(upload_id)
            .await
            .map_err(map_multipart_meta_err)
    }

    /// Validates `requested_parts` against recorded `RecordPart` history and, on
    /// success, atomically commits the concatenated result as the new current version
    /// via a single `CommitManifest` (architecture.md §15) -- the same atomic-visibility
    /// boundary a normal PUT uses, not a separate protocol. `content_type`/
    /// `user_metadata` come from the upload's own record (`CreateMultipartUpload`'s
    /// request), not this request: real `CompleteMultipartUpload` requests don't carry
    /// them.
    pub async fn complete_multipart_upload(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        requested_parts: Vec<(u32, String)>,
        requesting_owner: OwnerId,
    ) -> Result<ObjectManifest, Ls3Error> {
        let state = self
            .authorized_upload(bucket, key, upload_id, requesting_owner)
            .await?;
        self.metadata
            .complete_multipart(CompleteMultipart {
                upload_id,
                requested_parts,
                content_type: state.content_type,
            })
            .await
            .map_err(map_multipart_meta_err)
    }

    /// Marks the upload aborted; already-written part shards become orphan-GC
    /// candidates after the grace period, the same as any other unreferenced shard
    /// (architecture.md §15/§25 -- no synchronous shard deletion here, matching
    /// `delete_object`'s own reasoning).
    pub async fn abort_multipart_upload(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        requesting_owner: OwnerId,
    ) -> Result<(), Ls3Error> {
        self.authorized_upload(bucket, key, upload_id, requesting_owner)
            .await?;
        self.metadata
            .abort_multipart(upload_id)
            .await
            .map_err(map_multipart_meta_err)
    }

    pub async fn get_object(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        requesting_owner: OwnerId,
    ) -> Result<(ObjectManifest, ShardBytesOut), Ls3Error> {
        let manifest = self.head_object(bucket, key, requesting_owner).await?;
        let stripes: Vec<Stripe> = manifest
            .parts
            .iter()
            .flat_map(|p| p.stripes.iter().cloned())
            .collect();
        let shard_store = self.shard_store.clone();

        let stream = futures::stream::unfold(
            (stripes.into_iter(), shard_store),
            |(mut iter, shard_store)| async move {
                let stripe = iter.next()?;
                let result = fetch_and_reconstruct_stripe(&shard_store, &stripe)
                    .await
                    .map(Bytes::from);
                Some((result, (iter, shard_store)))
            },
        );

        let boxed: ShardBytesOut = Box::pin(stream);
        Ok((manifest, boxed))
    }

    pub async fn head_object(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        requesting_owner: OwnerId,
    ) -> Result<ObjectManifest, Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;
        self.metadata
            .get_manifest(bucket.bucket_id, key)
            .await?
            .ok_or(Ls3Error::NoSuchKey)
    }

    pub async fn delete_object(
        &self,
        bucket: &BucketName,
        key: &ObjectKey,
        requesting_owner: OwnerId,
    ) -> Result<(), Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;
        // Logical delete only (architecture.md §25/§104): shards this object
        // referenced are not touched here. They become orphan-GC candidates once a
        // healing/GC subsystem exists (a later phase); deleting them synchronously
        // here would risk resurrecting the object on a failed/partial cleanup.
        self.metadata
            .tombstone_object(bucket.bucket_id, key)
            .await?;
        Ok(())
    }

    pub async fn list_objects(
        &self,
        bucket: &BucketName,
        params: ListObjectsParams,
        requesting_owner: OwnerId,
    ) -> Result<ListObjectsPage, Ls3Error> {
        let bucket = self.authorized_bucket(bucket, requesting_owner).await?;
        Ok(self
            .metadata
            .list_objects(ListObjectsQuery {
                bucket_id: bucket.bucket_id,
                prefix: params.prefix,
                delimiter: params.delimiter,
                start_after: params.start_after,
                continuation_token: params.continuation_token,
                max_keys: params.max_keys,
            })
            .await?)
    }
}

/// Translates the multipart-specific [`MetaError`] variants (prompt §57's `NoSuchUpload`/
/// `InvalidPart`/`InvalidPartOrder`) into their dedicated [`Ls3Error`] counterparts so
/// `loony-api` maps them to the right HTTP status/code instead of a generic 500 — every
/// other variant still falls through to the blanket `Ls3Error::Meta` conversion. Needed
/// at each multipart metadata-store call past `authorized_upload`'s own check, since the
/// upload can still be concurrently aborted/completed by another request in between.
fn map_multipart_meta_err(err: MetaError) -> Ls3Error {
    match err {
        MetaError::NoSuchUpload(_) => Ls3Error::NoSuchUpload,
        MetaError::InvalidPart => Ls3Error::InvalidPart,
        MetaError::InvalidPartOrder => Ls3Error::InvalidPartOrder,
        other => other.into(),
    }
}

/// The client-supplied part of a ListObjectsV2 query (prompt §59) — everything except
/// which bucket and who's asking, which `list_objects` takes separately since every
/// other method here takes them the same way.
#[derive(Debug, Clone, Default)]
pub struct ListObjectsParams {
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    pub start_after: Option<String>,
    pub continuation_token: Option<String>,
    pub max_keys: u32,
}

/// Fetches and verifies enough shards of `stripe` to recover its bytes, reconstructing
/// via erasure decoding only when a data shard is actually missing/corrupt
/// (architecture.md §22/§23: degraded reads never block indefinitely and never
/// reconstruct unnecessarily).
async fn fetch_and_reconstruct_stripe(
    shard_store: &Arc<dyn ShardStore>,
    stripe: &Stripe,
) -> std::io::Result<Vec<u8>> {
    match stripe.durability {
        DurabilityPolicy::Replicated { .. } => {
            for shard in &stripe.shards {
                if let Ok(bytes) = fetch_and_verify_shard(shard_store, shard).await {
                    return Ok(bytes);
                }
            }
            Err(std::io::Error::other(
                "all replicas of this stripe are unavailable or corrupt",
            ))
        }
        DurabilityPolicy::Erasure { data, parity } => {
            let total = data as usize + parity as usize;
            let mut shards: Vec<Option<Vec<u8>>> = vec![None; total];
            let mut shard_len = 0usize;
            for loc in &stripe.shards {
                if let Ok(bytes) = fetch_and_verify_shard(shard_store, loc).await {
                    shard_len = shard_len.max(bytes.len());
                    if let Some(slot) = shards.get_mut(loc.shard_index as usize) {
                        *slot = Some(bytes);
                    }
                }
            }
            let codec = RsErasureCodec::new(data as usize, parity as usize);
            codec
                .reconstruct(&shards, shard_len, stripe.stripe_len as usize)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }
    }
}

/// Reads one shard fully and checks it against the manifest's recorded checksum
/// (architecture.md §30/§103) — a shard that fails this check is treated exactly like
/// an unreachable one by the caller, never served.
async fn fetch_and_verify_shard(
    shard_store: &Arc<dyn ShardStore>,
    loc: &ShardLocation,
) -> std::io::Result<Vec<u8>> {
    use sha2::Digest as _;

    let target = ShardTarget {
        node_id: loc.node_id,
        volume_id: loc.volume_id,
    };
    let mut stream = shard_store
        .get_shard(target, loc.shard_id)
        .await
        .map_err(std::io::Error::other)?;
    let mut buf = Vec::with_capacity(loc.size as usize);
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    let actual: [u8; 32] = Sha256::digest(&buf).into();
    if actual != loc.checksum {
        return Err(std::io::Error::other(format!(
            "shard {} failed checksum verification",
            loc.shard_id
        )));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loony_metadata::{CreateBucket, RedbMetadataStore};
    use loony_storage::LocalVolumeManager;

    async fn service_with_volumes(volume_count: usize) -> (ObjectService, BucketName, OwnerId) {
        let meta_dir = tempfile::tempdir().unwrap();
        let metadata = Arc::new(
            RedbMetadataStore::open(meta_dir.keep().join("meta.redb"))
                .await
                .unwrap(),
        );
        let node_id = NodeId::new();
        let mut roots = Vec::new();
        for _ in 0..volume_count {
            roots.push(tempfile::tempdir().unwrap().keep());
        }
        let volumes = Arc::new(LocalVolumeManager::open(node_id, roots).await.unwrap());
        let volume_ids: Vec<VolumeId> = volumes.volume_ids().collect();

        let name = BucketName::parse("test-bucket").unwrap();
        let owner = OwnerId::new();
        metadata
            .create_bucket(CreateBucket {
                name: name.clone(),
                owner_id: owner,
                region: "us-east-1".into(),
            })
            .await
            .unwrap();

        (
            ObjectService::new(metadata, volumes, node_id, volume_ids),
            name,
            owner,
        )
    }

    async fn service() -> (ObjectService, BucketName, OwnerId) {
        service_with_volumes(1).await
    }

    fn body(data: &'static [u8]) -> ShardBytesIn {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(data))]))
    }

    async fn collect(mut stream: ShardBytesOut) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn small_object_put_get_head_delete_roundtrip() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("hello.txt").unwrap();

        let manifest = service
            .put_object(
                &bucket,
                key.clone(),
                "text/plain".into(),
                Default::default(),
                body(b"hello world"),
                owner,
            )
            .await
            .unwrap();
        assert_eq!(manifest.size, 11);
        assert_eq!(manifest.etag.as_str().len(), 32);
        assert!(matches!(
            manifest.parts[0].stripes[0].durability,
            DurabilityPolicy::Replicated { .. }
        ));

        let headed = service.head_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(headed.object_id, manifest.object_id);

        let (fetched, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(fetched.object_id, manifest.object_id);
        assert_eq!(collect(stream).await, b"hello world");

        service.delete_object(&bucket, &key, owner).await.unwrap();
        let err = service.head_object(&bucket, &key, owner).await.unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchKey));
    }

    #[tokio::test]
    async fn large_object_is_erasure_coded_across_multiple_volumes_and_roundtrips() {
        let (service, bucket, owner) = service_with_volumes(6).await;
        let key = ObjectKey::parse("big.bin").unwrap();

        // Bigger than the small-object threshold and spans multiple stripes.
        let mut data = Vec::new();
        for i in 0..(3 * 1024 * 1024usize) {
            data.push((i % 251) as u8);
        }
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let stream: ShardBytesIn = {
            // Feed it in multiple chunks to exercise the stripe reader's leftover path.
            let chunks: Vec<_> = data
                .chunks(64 * 1024)
                .map(|c| Ok(Bytes::from_static(c)))
                .collect();
            Box::pin(futures::stream::iter(chunks))
        };

        let manifest = service
            .put_object(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                stream,
                owner,
            )
            .await
            .unwrap();
        assert_eq!(manifest.size, data.len() as u64);
        match manifest.parts[0].stripes[0].durability {
            DurabilityPolicy::Erasure { data: d, parity: p } => {
                assert_eq!((d, p), (4, 2));
            }
            other => panic!("expected Erasure durability, got {other:?}"),
        }

        let (_, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(collect(stream).await, data);
    }

    #[tokio::test]
    async fn degraded_read_survives_a_missing_shard_file() {
        let (service, bucket, owner) = service_with_volumes(6).await;
        let key = ObjectKey::parse("resilient.bin").unwrap();

        let mut data = vec![0u8; 2 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 253) as u8;
        }
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());

        let manifest = service
            .put_object(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                body(data),
                owner,
            )
            .await
            .unwrap();

        // Simulate losing one shard (e.g. a disk failure) by deleting its physical
        // file directly through the shard store -- bypassing the object service, the
        // way an out-of-band failure would.
        let victim = &manifest.parts[0].stripes[0].shards[0];
        let target = ShardTarget {
            node_id: victim.node_id,
            volume_id: victim.volume_id,
        };
        service
            .shard_store
            .delete_shard(target, victim.shard_id)
            .await
            .unwrap();

        let (_, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(collect(stream).await, data);
    }

    #[tokio::test]
    async fn put_to_missing_bucket_is_rejected() {
        let (service, _bucket, owner) = service().await;
        let missing = BucketName::parse("does-not-exist").unwrap();
        let err = service
            .put_object(
                &missing,
                ObjectKey::parse("a").unwrap(),
                "text/plain".into(),
                Default::default(),
                body(b"x"),
                owner,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchBucket));
    }

    #[tokio::test]
    async fn a_different_owner_cannot_read_or_write_the_bucket() {
        let (service, bucket, owner) = service().await;
        let other = OwnerId::new();
        let key = ObjectKey::parse("k").unwrap();

        service
            .put_object(
                &bucket,
                key.clone(),
                "text/plain".into(),
                Default::default(),
                body(b"x"),
                owner,
            )
            .await
            .unwrap();

        let err = service
            .put_object(
                &bucket,
                key.clone(),
                "text/plain".into(),
                Default::default(),
                body(b"y"),
                other,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));

        let err = service
            .get_object(&bucket, &key, other)
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));

        let err = service
            .delete_object(&bucket, &key, other)
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));

        service.head_object(&bucket, &key, owner).await.unwrap();
    }

    #[tokio::test]
    async fn overwriting_a_key_replaces_what_get_returns() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("k").unwrap();

        service
            .put_object(
                &bucket,
                key.clone(),
                "text/plain".into(),
                Default::default(),
                body(b"first"),
                owner,
            )
            .await
            .unwrap();
        service
            .put_object(
                &bucket,
                key.clone(),
                "text/plain".into(),
                Default::default(),
                body(b"second, longer"),
                owner,
            )
            .await
            .unwrap();

        let (manifest, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(collect(stream).await, b"second, longer");
        assert_eq!(manifest.size, 14);
    }

    #[tokio::test]
    async fn list_objects_delegates_prefix_and_max_keys() {
        let (service, bucket, owner) = service().await;
        for key in ["a/1", "a/2", "b/1"] {
            service
                .put_object(
                    &bucket,
                    ObjectKey::parse(key).unwrap(),
                    "text/plain".into(),
                    Default::default(),
                    body(b"x"),
                    owner,
                )
                .await
                .unwrap();
        }

        let params = ListObjectsParams {
            prefix: Some("a/".into()),
            max_keys: 100,
            ..Default::default()
        };
        let page = service.list_objects(&bucket, params, owner).await.unwrap();
        assert_eq!(page.objects.len(), 2);
    }

    #[tokio::test]
    async fn multipart_upload_uploads_parts_and_completes_into_one_object() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("big.bin").unwrap();

        let upload_id = service
            .create_multipart_upload(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                BTreeMap::from([("origin".to_string(), "test".to_string())]),
                owner,
            )
            .await
            .unwrap();

        let etag1 = service
            .upload_part(&bucket, &key, upload_id, 1, body(b"hello "), owner)
            .await
            .unwrap();
        let etag2 = service
            .upload_part(&bucket, &key, upload_id, 2, body(b"world"), owner)
            .await
            .unwrap();

        let parts = service
            .list_parts(&bucket, &key, upload_id, owner)
            .await
            .unwrap();
        assert_eq!(parts.len(), 2);

        let manifest = service
            .complete_multipart_upload(
                &bucket,
                &key,
                upload_id,
                vec![
                    (1, etag1.as_str().to_string()),
                    (2, etag2.as_str().to_string()),
                ],
                owner,
            )
            .await
            .unwrap();
        assert_eq!(manifest.size, 11);
        assert!(manifest.etag.as_str().ends_with("-2"));
        assert_eq!(manifest.content_type, "application/octet-stream");
        assert_eq!(
            manifest.user_metadata.get("origin"),
            Some(&"test".to_string())
        );

        let (headed, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        assert_eq!(headed.object_id, manifest.object_id);
        assert_eq!(collect(stream).await, b"hello world");

        // The completed upload no longer exists.
        let err = service
            .list_parts(&bucket, &key, upload_id, owner)
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchUpload));
    }

    #[tokio::test]
    async fn aborting_a_multipart_upload_discards_it() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("abandoned.bin").unwrap();

        let upload_id = service
            .create_multipart_upload(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                owner,
            )
            .await
            .unwrap();
        service
            .upload_part(&bucket, &key, upload_id, 1, body(b"x"), owner)
            .await
            .unwrap();

        service
            .abort_multipart_upload(&bucket, &key, upload_id, owner)
            .await
            .unwrap();

        let err = service
            .list_parts(&bucket, &key, upload_id, owner)
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchUpload));
    }

    #[tokio::test]
    async fn completing_with_a_wrong_etag_or_out_of_order_parts_is_rejected() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("k").unwrap();

        let upload_id = service
            .create_multipart_upload(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                owner,
            )
            .await
            .unwrap();
        let etag1 = service
            .upload_part(&bucket, &key, upload_id, 1, body(b"a"), owner)
            .await
            .unwrap();
        let etag2 = service
            .upload_part(&bucket, &key, upload_id, 2, body(b"b"), owner)
            .await
            .unwrap();

        let err = service
            .complete_multipart_upload(
                &bucket,
                &key,
                upload_id,
                vec![(1, "deadbeefdeadbeefdeadbeefdeadbeef".to_string())],
                owner,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::InvalidPart));

        let err = service
            .complete_multipart_upload(
                &bucket,
                &key,
                upload_id,
                vec![
                    (2, etag2.as_str().to_string()),
                    (1, etag1.as_str().to_string()),
                ],
                owner,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::InvalidPartOrder));
    }

    #[tokio::test]
    async fn multipart_operations_reject_a_different_owners_bucket() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("k").unwrap();
        let upload_id = service
            .create_multipart_upload(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                owner,
            )
            .await
            .unwrap();

        let other = OwnerId::new();
        let err = service
            .upload_part(&bucket, &key, upload_id, 1, body(b"x"), other)
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::AccessDenied));
    }

    /// Splits `data` into many small chunks the way a real streamed HTTP request body
    /// arrives (never one single `Bytes` blob, unlike the `body()` helper above) -- the
    /// live multipart duplication bug this test was written to catch only reproduced
    /// over a real network, not through `body()`'s single-chunk stream.
    fn chunked_body(data: &'static [u8]) -> ShardBytesIn {
        let chunks: Vec<_> = data
            .chunks(64 * 1024)
            .map(|c| Ok(Bytes::from_static(c)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }

    #[tokio::test]
    async fn multipart_upload_with_erasure_coded_parts_roundtrips_without_duplication() {
        let (service, bucket, owner) = service_with_volumes(6).await;
        let key = ObjectKey::parse("big-multipart.bin").unwrap();

        // Each part exceeds SMALL_OBJECT_THRESHOLD, so every part goes through the
        // erasure-coded path individually, the way a real multi-MB multipart part does.
        let mut part1_data = vec![0u8; 5 * 1024 * 1024];
        for (i, b) in part1_data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let part1_data: &'static [u8] = Box::leak(part1_data.into_boxed_slice());
        let mut part2_data = vec![0u8; 5 * 1024 * 1024];
        for (i, b) in part2_data.iter_mut().enumerate() {
            *b = ((i + 7) % 251) as u8;
        }
        let part2_data: &'static [u8] = Box::leak(part2_data.into_boxed_slice());

        let upload_id = service
            .create_multipart_upload(
                &bucket,
                key.clone(),
                "application/octet-stream".into(),
                Default::default(),
                owner,
            )
            .await
            .unwrap();
        let etag1 = service
            .upload_part(&bucket, &key, upload_id, 1, chunked_body(part1_data), owner)
            .await
            .unwrap();
        let etag2 = service
            .upload_part(&bucket, &key, upload_id, 2, chunked_body(part2_data), owner)
            .await
            .unwrap();

        let manifest = service
            .complete_multipart_upload(
                &bucket,
                &key,
                upload_id,
                vec![
                    (1, etag1.as_str().to_string()),
                    (2, etag2.as_str().to_string()),
                ],
                owner,
            )
            .await
            .unwrap();
        assert_eq!(manifest.size, (part1_data.len() + part2_data.len()) as u64);

        let (_, stream) = service.get_object(&bucket, &key, owner).await.unwrap();
        let fetched = collect(stream).await;
        assert_eq!(fetched.len(), part1_data.len() + part2_data.len());
        let mut expected = part1_data.to_vec();
        expected.extend_from_slice(part2_data);
        assert_eq!(fetched, expected);
    }

    #[tokio::test]
    async fn an_unknown_upload_id_is_rejected() {
        let (service, bucket, owner) = service().await;
        let key = ObjectKey::parse("k").unwrap();
        let err = service
            .list_parts(&bucket, &key, UploadId::new(), owner)
            .await
            .unwrap_err();
        assert!(matches!(err, Ls3Error::NoSuchUpload));
    }
}
