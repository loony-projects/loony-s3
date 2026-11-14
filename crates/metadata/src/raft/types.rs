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
    Bucket, BucketId, BucketName, CheckoutId, ClusterId, GiftCodeId, InviteId, LedgerEntryId,
    NodeId, NodeInfo, NodeState, ObjectId, ObjectKey, ObjectManifest, OwnerId, PartManifest,
    ProjectId, UploadId, UserId, VersionId,
};
use time::OffsetDateTime;

use crate::commands::{
    AdoptOwner, BeginMultipart, CompleteMultipart, CreateBucket, CreateInvite, CreateProject,
    Credential, Project, ProjectInvite, ProjectMember, ProjectRole, RegisterNode, User, UserAccess,
    UserSignIn,
};
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

/// `user_id` only takes effect the first time this issuer and subject are seen, so two
/// simultaneous first sign-ins still end up as one user.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedUserSignIn {
    pub sign_in: UserSignIn,
    pub user_id: UserId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedPay {
    pub project_id: ProjectId,
    pub payer: UserId,
    pub plan: crate::billing::Plan,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub entry_id: LedgerEntryId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCancel {
    pub project_id: ProjectId,
    pub user_id: UserId,
    pub cancel: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// The state the background task saw, so applying the step twice is a no-op.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedAdvance {
    pub project_id: ProjectId,
    pub expected_state: crate::billing::SubscriptionState,
    #[serde(with = "time::serde::rfc3339")]
    pub expected_period_end: OffsetDateTime,
    pub timings: crate::billing::BillingTimings,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub entry_id: LedgerEntryId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedNewGiftCode {
    pub code: crate::billing::NewGiftCode,
    pub code_id: GiftCodeId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedRedeem {
    pub code_hash: String,
    pub user_id: UserId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub entry_id: LedgerEntryId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedNewCheckout {
    pub checkout: crate::billing::NewCheckout,
    pub checkout_id: CheckoutId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCompleteCheckout {
    pub checkout_id: CheckoutId,
    pub succeeded: bool,
    pub provider_ref: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub entry_id: LedgerEntryId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCreateProject {
    pub cmd: CreateProject,
    pub project_id: ProjectId,
    pub owner_id: OwnerId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedAdoptOwner {
    pub cmd: AdoptOwner,
    pub project_id: ProjectId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedCreateInvite {
    pub cmd: CreateInvite,
    pub invite_id: InviteId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Answering an invite carries the time because whether it has expired must come out the
/// same on every node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedInviteAnswer {
    pub invite_id: InviteId,
    pub user_id: UserId,
    pub accept: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedTouchCredential {
    pub access_key: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
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
    DeleteCredential(String),
    RecordUserSignIn(Box<ResolvedUserSignIn>),
    SetUserAccess(UserId, UserAccess),
    CreateProject(Box<ResolvedCreateProject>),
    AdoptOwner(Box<ResolvedAdoptOwner>),
    RenameProject(ProjectId, String),
    SetProjectEnabled(ProjectId, bool),
    DeleteProject(ProjectId),
    SetMemberRole(ProjectId, UserId, ProjectRole),
    RemoveMember(ProjectId, UserId),
    CreateInvite(Box<ResolvedCreateInvite>),
    DeleteInvite(InviteId),
    AnswerInvite(Box<ResolvedInviteAnswer>),
    PutProjectCredential(Box<Credential>, u32),
    SetCredentialEnabled(String, bool),
    TouchCredential(Box<ResolvedTouchCredential>),
    DeleteSession(String),
    PayForProject(Box<ResolvedPay>),
    SetCancel(Box<ResolvedCancel>),
    AdvanceSubscription(Box<ResolvedAdvance>),
    CreateGiftCode(Box<ResolvedNewGiftCode>),
    SetGiftCodeDisabled(GiftCodeId, bool),
    RedeemGiftCode(Box<ResolvedRedeem>),
    CreateCheckout(Box<ResolvedNewCheckout>),
    CompleteCheckout(Box<ResolvedCompleteCheckout>),
    /// Builds usage counters and indexes from the data, once, for data stored before them.
    RebuildDerivedIndexes,
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
    User(Box<User>),
    Project(Box<Project>),
    Member(Box<ProjectMember>),
    Invite(Box<ProjectInvite>),
    Paid(Box<(crate::billing::Subscription, crate::billing::LedgerEntry)>),
    Subscription(Box<crate::billing::Subscription>),
    Advanced(Box<Option<(crate::billing::Subscription, crate::billing::Step)>>),
    GiftCode(Box<crate::billing::GiftCode>),
    Redeemed(Box<(crate::billing::GiftCode, crate::billing::LedgerEntry)>),
    Checkout(Box<crate::billing::Checkout>),
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
