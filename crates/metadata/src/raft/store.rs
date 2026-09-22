//! `RaftMetadataStore`: the `MetadataStore` implementation every phase-8-and-later node
//! runs, wrapping `openraft` around exactly the state machine Phase 2's
//! `RedbMetadataStore` implemented directly (architecture.md §5, `lib.rs`'s module doc).
//! Mutating trait methods go through `Raft::client_write` — real consensus even for a
//! single-voter group, so standalone and cluster mode are, for the first time, running
//! the literal same code path rather than an equivalence argument about it. Read methods
//! bypass Raft and query the state machine's redb tables directly (`applier.rs`) — the
//! same "local reads, replicated writes" simplification `raft-kv-memstore`'s own `/read`
//! handler makes, and standard for Raft-backed stores that don't need linearizable reads
//! on followers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use openraft::{BasicNode, Config, Raft};
use redb::Database;
use loony_core::{
    Bucket, BucketId, BucketName, ClusterId, NodeId, NodeInfo, NodeState, ObjectId, ObjectKey,
    ObjectManifest, OwnerId, PartManifest, UploadId, VersionId,
};

use crate::commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, Credential, ListObjectsPage, ListObjectsQuery,
    MultipartUploadState, PartSummary, RegisterNode,
};
use crate::error::MetaError;
use crate::raft::applier::{self, db_err};
use crate::raft::log_store::RedbLogStore;
use crate::raft::state_machine::RedbStateMachineStore;
use crate::raft::types::{
    CommandResponse, MetadataCommand, ResolvedBeginMultipart, ResolvedCompleteMultipart,
    ResolvedCreateBucket, TypeConfig,
};
use crate::store::MetadataStore;

pub struct RaftMetadataStore {
    raft: Raft<TypeConfig>,
    db: Arc<Database>,
}

fn create_tables(db: &Database) -> Result<(), MetaError> {
    let write_txn = db.begin_write().map_err(db_err)?;
    for table in applier::ALL_TABLES {
        write_txn.open_table(*table).map_err(db_err)?;
    }
    // Raft's own tables (log_store.rs / state_machine.rs) -- opened here too so a
    // brand-new data directory has every table present before `Raft::new` touches any
    // of them.
    write_txn
        .open_table(redb::TableDefinition::<u64, &[u8]>::new("raft_log"))
        .map_err(db_err)?;
    write_txn
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("raft_vote"))
        .map_err(db_err)?;
    write_txn
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("raft_last_purged"))
        .map_err(db_err)?;
    write_txn
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("sm_meta"))
        .map_err(db_err)?;
    write_txn
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("snapshot"))
        .map_err(db_err)?;
    write_txn.commit().map_err(db_err)?;
    Ok(())
}

impl RaftMetadataStore {
    /// Opens (creating if absent) the redb file at `path` and constructs a `Raft`
    /// instance over it, using `network` to reach any other voters. Does **not**
    /// initialize the group — a pristine node stays uninitialized (and
    /// `client_write`/reads of not-yet-existing data behave accordingly) until the
    /// caller calls [`Self::raft`]`.initialize(...)` — see [`Self::open_single_node`]
    /// for the standalone convenience path that does this automatically.
    pub async fn open<N>(node_id: NodeId, path: impl Into<PathBuf>, network: N) -> Result<Self, MetaError>
    where
        N: openraft::network::RaftNetworkFactory<TypeConfig>,
    {
        let path = path.into();
        let db = tokio::task::spawn_blocking(move || -> Result<Database, MetaError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(db_err)?;
            }
            let db = Database::create(&path).map_err(db_err)?;
            create_tables(&db)?;
            Ok(db)
        })
        .await
        .map_err(|e| MetaError::TaskPanicked(e.to_string()))??;
        let db = Arc::new(db);

        let config = Arc::new(
            Config::default()
                .validate()
                .map_err(|e| MetaError::Db(format!("invalid raft config: {e}")))?,
        );
        let log_store = RedbLogStore::new(db.clone());
        let state_machine = RedbStateMachineStore::new(db.clone());

        let raft = Raft::new(node_id, config, network, log_store, state_machine)
            .await
            .map_err(|e| MetaError::Db(format!("raft engine failed to start: {e}")))?;

        Ok(Self { raft, db })
    }

    /// Standalone mode's constructor (architecture.md §2): a single-voter group,
    /// initialized to itself on first boot. A restart of an already-initialized node is
    /// a no-op here, not an error -- `Raft::initialize` on an already-initialized group
    /// returns `InitializeError::NotAllowed`, which is exactly "this is not a pristine
    /// node," i.e. expected on every boot after the first.
    pub async fn open_single_node(
        node_id: NodeId,
        advertised_address: String,
        path: impl Into<PathBuf>,
    ) -> Result<Self, MetaError> {
        let store = Self::open(node_id, path, crate::raft::network::NoopNetworkFactory).await?;

        if !store
            .raft
            .is_initialized()
            .await
            .map_err(|e| MetaError::Db(format!("raft engine is unavailable: {e}")))?
        {
            let mut members = BTreeMap::new();
            members.insert(node_id, BasicNode::new(advertised_address));
            if let Err(err) = store.raft.initialize(members).await {
                tracing::debug!(%err, "raft initialize: already initialized (expected on restart)");
            }
        }

        Ok(store)
    }

    /// The underlying `Raft` handle -- membership changes (`add_learner`,
    /// `change_membership`), metrics, and multi-voter `initialize` all go through this
    /// directly rather than through `MetadataStore` (which only covers the
    /// application-level commands every voter count shares).
    pub fn raft(&self) -> &Raft<TypeConfig> {
        &self.raft
    }

    async fn read<F, T>(&self, f: F) -> Result<T, MetaError>
    where
        F: FnOnce(&Database) -> Result<T, MetaError> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| MetaError::TaskPanicked(e.to_string()))?
    }

    async fn write(&self, cmd: MetadataCommand) -> Result<CommandResponse, MetaError> {
        // `Raft::client_write` has no built-in timeout: proposed on a leader that has
        // since lost quorum (a network partition, not a graceful step-down), it can
        // block forever waiting for an ack that will never arrive, rather than erroring
        // -- openraft only reports "not the leader" when the node already *knows* it
        // isn't one. Without this bound, one write to a partitioned-away former leader
        // would hang the HTTP request that triggered it (and the Tokio task under it)
        // indefinitely.
        let response = tokio::time::timeout(WRITE_TIMEOUT, self.raft.client_write(cmd))
            .await
            .map_err(|_| MetaError::RaftUnavailable("write timed out waiting for consensus".into()))?
            .map_err(|e| MetaError::RaftUnavailable(e.to_string()))?;
        response.data
    }
}

/// How long a write waits for Raft consensus before giving up (see `write`'s doc
/// comment). Comfortably above the default election timeout (150-300ms) so a normal
/// leader failover during a write still succeeds once the new leader catches up.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn unexpected_response() -> MetaError {
    MetaError::Db("raft state machine returned an unexpected response variant".into())
}

#[async_trait]
impl MetadataStore for RaftMetadataStore {
    async fn create_bucket(&self, cmd: CreateBucket) -> Result<Bucket, MetaError> {
        // The bucket's id and creation time are resolved *here*, once, before the
        // command is proposed -- never inside `apply()`, which every voter/learner
        // runs independently and which must therefore be fully deterministic. See
        // `ResolvedCreateBucket`'s doc comment for how this went wrong before it was
        // fixed.
        let resolved = ResolvedCreateBucket {
            cmd,
            bucket_id: BucketId::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };
        match self.write(MetadataCommand::CreateBucket(resolved)).await? {
            CommandResponse::Bucket(b) => Ok(*b),
            _ => Err(unexpected_response()),
        }
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<(), MetaError> {
        self.write(MetadataCommand::DeleteBucket(name.clone())).await?;
        Ok(())
    }

    async fn get_bucket(&self, name: &BucketName) -> Result<Option<Bucket>, MetaError> {
        let name = name.clone();
        self.read(move |db| applier::get_bucket(db, &name)).await
    }

    async fn list_buckets(&self, owner: OwnerId) -> Result<Vec<Bucket>, MetaError> {
        self.read(move |db| applier::list_buckets(db, owner)).await
    }

    async fn commit_manifest(&self, manifest: ObjectManifest) -> Result<ObjectManifest, MetaError> {
        match self
            .write(MetadataCommand::CommitManifest(Box::new(manifest)))
            .await?
        {
            CommandResponse::Manifest(m) => Ok(*m),
            _ => Err(unexpected_response()),
        }
    }

    async fn get_manifest(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectManifest>, MetaError> {
        let key = key.clone();
        self.read(move |db| applier::get_manifest(db, bucket_id, &key)).await
    }

    async fn tombstone_object(&self, bucket_id: BucketId, key: &ObjectKey) -> Result<(), MetaError> {
        self.write(MetadataCommand::TombstoneObject(bucket_id, key.clone()))
            .await?;
        Ok(())
    }

    async fn list_objects(&self, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError> {
        self.read(move |db| applier::list_objects(db, query)).await
    }

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError> {
        let resolved = ResolvedBeginMultipart {
            cmd,
            upload_id: UploadId::new(),
            initiated_at: time::OffsetDateTime::now_utc(),
        };
        match self.write(MetadataCommand::BeginMultipart(resolved)).await? {
            CommandResponse::UploadId(id) => Ok(id),
            _ => Err(unexpected_response()),
        }
    }

    async fn get_upload(&self, upload_id: UploadId) -> Result<Option<MultipartUploadState>, MetaError> {
        self.read(move |db| applier::get_upload(db, upload_id)).await
    }

    async fn record_part(&self, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError> {
        self.write(MetadataCommand::RecordPart(upload_id, Box::new(part)))
            .await?;
        Ok(())
    }

    async fn list_parts(&self, upload_id: UploadId) -> Result<Vec<PartSummary>, MetaError> {
        self.read(move |db| applier::list_parts(db, upload_id)).await
    }

    async fn complete_multipart(&self, cmd: CompleteMultipart) -> Result<ObjectManifest, MetaError> {
        let resolved = ResolvedCompleteMultipart {
            cmd,
            object_id: ObjectId::new(),
            version_id: VersionId::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };
        match self
            .write(MetadataCommand::CompleteMultipart(Box::new(resolved)))
            .await?
        {
            CommandResponse::Manifest(m) => Ok(*m),
            _ => Err(unexpected_response()),
        }
    }

    async fn abort_multipart(&self, upload_id: UploadId) -> Result<(), MetaError> {
        self.write(MetadataCommand::AbortMultipart(upload_id)).await?;
        Ok(())
    }

    async fn put_credential(&self, cred: Credential) -> Result<(), MetaError> {
        self.write(MetadataCommand::PutCredential(Box::new(cred))).await?;
        Ok(())
    }

    async fn get_credential(&self, access_key: &str) -> Result<Option<Credential>, MetaError> {
        let access_key = access_key.to_string();
        self.read(move |db| applier::get_credential(db, &access_key)).await
    }

    async fn bootstrap_cluster(&self, cluster_id: ClusterId) -> Result<(), MetaError> {
        self.write(MetadataCommand::BootstrapCluster(cluster_id)).await?;
        Ok(())
    }

    async fn get_cluster_id(&self) -> Result<Option<ClusterId>, MetaError> {
        self.read(applier::get_cluster_id).await
    }

    async fn register_node(&self, cmd: RegisterNode) -> Result<NodeInfo, MetaError> {
        match self.write(MetadataCommand::RegisterNode(cmd)).await? {
            CommandResponse::NodeInfo(n) => Ok(*n),
            _ => Err(unexpected_response()),
        }
    }

    async fn update_node_state(&self, node_id: NodeId, state: NodeState) -> Result<(), MetaError> {
        self.write(MetadataCommand::UpdateNodeState(node_id, state)).await?;
        Ok(())
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, MetaError> {
        self.read(applier::list_nodes).await
    }
}
