//! The `openraft` type configuration (Phase 8, architecture.md §5): the shapes that
//! flow through the Raft log, and the state-machine's application-level response.
//!
//! `NodeId` is `s3_core::NodeId` directly — it already has every derive `openraft`
//! requires (architecture.md §34: node identity is minted once and persisted, never
//! derived from a network address, which is exactly the property a Raft voter id
//! needs). `Node` is `openraft::BasicNode`, which is exactly "an advertised address"
//! (architecture.md §17) and nothing more.

use std::io::Cursor;

use s3_core::{
    Bucket, BucketId, BucketName, ClusterId, NodeId, NodeInfo, NodeState, ObjectKey,
    ObjectManifest, PartManifest, UploadId,
};

use crate::commands::{BeginMultipart, CompleteMultipart, CreateBucket, Credential, RegisterNode};
use crate::error::MetaError;

/// Every mutation `MetadataStore` exposes, shaped as one flat enum so it can be a Raft
/// log entry's application payload (`commands.rs`'s doc comment anticipated exactly
/// this since Phase 2). Each variant mirrors one `MetadataStore` method 1:1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MetadataCommand {
    CreateBucket(CreateBucket),
    DeleteBucket(BucketName),
    CommitManifest(Box<ObjectManifest>),
    TombstoneObject(BucketId, ObjectKey),
    BeginMultipart(BeginMultipart),
    RecordPart(UploadId, Box<PartManifest>),
    CompleteMultipart(CompleteMultipart),
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
