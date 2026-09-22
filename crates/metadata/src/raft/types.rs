//! The `openraft` type configuration (Phase 8, architecture.md §5): the shapes that
//! flow through the Raft log, and the state-machine's application-level response.
//!
//! `NodeId` is `loony_core::NodeId` directly — it already has every derive `openraft`
//! requires (architecture.md §34: node identity is minted once and persisted, never
//! derived from a network address, which is exactly the property a Raft voter id
//! needs). `Node` is `openraft::BasicNode`, which is exactly "an advertised address"
//! (architecture.md §17) and nothing more.

use std::io::Cursor;

use loony_core::{
    Bucket, BucketId, BucketName, ClusterId, NodeId, NodeInfo, NodeState, ObjectId, ObjectKey,
    ObjectManifest, PartManifest, UploadId, VersionId,
};
use time::OffsetDateTime;

use crate::commands::{BeginMultipart, CompleteMultipart, CreateBucket, Credential, RegisterNode};
use crate::error::MetaError;

/// A [`MetadataCommand`] must be applied deterministically: every voter/learner runs
/// the exact same command through `apply()` and must reach the exact same state. Any ID
/// or timestamp a command's result needs has to be resolved *once* -- by whichever node
/// first proposes the command via `Raft::client_write` -- and carried inside the command
/// itself, never minted fresh inside `apply()`. Minting there (`BucketId::new()` et al.)
/// would generate a *different* value on every replica that replays the command, which
/// is silently invisible with a single voter (the one application of `apply()` *is* the
/// only copy) and a real correctness bug the moment a second voter/learner exists: the
/// non-deterministic run diverges from the leader's, e.g. `RaftMetadataStore::commit_manifest`
/// embeds the leader's `bucket_id` directly, but a `CreateBucket` replayed on a learner
/// that minted its own `BucketId` internally would leave that manifest pointing at a row
/// that learner's own bucket table doesn't have -- found via a real 2-node join test,
/// not by inspection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCreateBucket {
    pub cmd: CreateBucket,
    pub bucket_id: BucketId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedBeginMultipart {
    pub cmd: BeginMultipart,
    pub upload_id: UploadId,
    #[serde(with = "time::serde::rfc3339")]
    pub initiated_at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCompleteMultipart {
    pub cmd: CompleteMultipart,
    pub object_id: ObjectId,
    pub version_id: VersionId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Every mutation `MetadataStore` exposes, shaped as one flat enum so it can be a Raft
/// log entry's application payload (`commands.rs`'s doc comment anticipated exactly
/// this since Phase 2). Each variant mirrors one `MetadataStore` method 1:1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MetadataCommand {
    CreateBucket(ResolvedCreateBucket),
    DeleteBucket(BucketName),
    CommitManifest(Box<ObjectManifest>),
    TombstoneObject(BucketId, ObjectKey),
    BeginMultipart(ResolvedBeginMultipart),
    RecordPart(UploadId, Box<PartManifest>),
    CompleteMultipart(Box<ResolvedCompleteMultipart>),
    AbortMultipart(UploadId),
    PutCredential(Box<Credential>),
    BootstrapCluster(ClusterId),
    RegisterNode(RegisterNode),
    UpdateNodeState(NodeId, NodeState),
}

/// The state machine's response to one applied [`MetadataCommand`] — `openraft`'s
/// `C::R`. Every variant matches the return type of the `MetadataStore` method that
/// produced it, so [`crate::raft::RaftMetadataStore`] can unwrap exactly the variant it
/// expects without a fallible cast.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum CommandResponse {
    Bucket(Box<Bucket>),
    Manifest(Box<ObjectManifest>),
    UploadId(UploadId),
    NodeInfo(Box<NodeInfo>),
    Unit,
}

/// `openraft`'s `C::R`: the outer `Result` carries *business* outcomes (a duplicate
/// bucket name, an unknown upload id, ...) through the same commit path a success takes
/// — `openraft`'s own `RaftError`/`ClientWriteError` are reserved for Raft-infra
/// failures (not the leader, lost quorum), which `RaftMetadataStore` maps to
/// [`MetaError::RaftUnavailable`] separately. See `error.rs`'s doc comment on
/// `MetaError` for why this requires `MetaError: Clone + Serialize + Deserialize`.
pub type CommandResult = Result<CommandResponse, MetaError>;

openraft::declare_raft_types!(
    pub TypeConfig:
        D = MetadataCommand,
        R = CommandResult,
        NodeId = NodeId,
        Node = openraft::BasicNode,
);

pub type Raft = openraft::Raft<TypeConfig>;
