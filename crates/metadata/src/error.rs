/// Clone + Serialize/Deserialize so a business-logic failure (e.g. `BucketAlreadyExists`)
/// can travel back through `openraft`'s `client_write` response channel as `C::R`
/// (Phase 8) exactly like a successful result does — not just infra-level failures like
/// "not the leader", which `openraft` reports separately via `RaftError`.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum MetaError {
    #[error("bucket {0:?} does not exist")]
    NoSuchBucket(String),

    #[error("bucket {0:?} already exists")]
    BucketAlreadyExists(String),

    #[error("bucket {0:?} is not empty")]
    BucketNotEmpty(String),

    #[error("the specified key does not exist: {0:?}")]
    NoSuchKey(String),

    #[error("upload {0} does not exist")]
    NoSuchUpload(String),

    #[error("one or more of the specified parts could not be found")]
    InvalidPart,

    #[error("the list of parts was not in ascending order")]
    InvalidPartOrder,

    #[error("the continuation token is not valid")]
    InvalidContinuationToken,

    #[error("user {0} does not exist")]
    NoSuchUser(String),

    #[error("this project's subscription is {0}, so it can't take on data")]
    ProjectNotWritable(crate::billing::SubscriptionState),

    #[error(
        "this would use more storage than the plan allows ({used} of {limit} bytes used, {requested} more requested)"
    )]
    QuotaExceeded {
        limit: u64,
        used: u64,
        requested: u64,
    },

    #[error("not enough credit: {balance} paise available, {needed} needed")]
    InsufficientCredit { balance: u64, needed: u64 },

    #[error("this project is already paid for")]
    AlreadyPaid,

    #[error("this project has no subscription to change")]
    NoSubscription,

    #[error("only a project Owner can do that")]
    NotProjectOwner,

    #[error("that gift code doesn't exist")]
    NoSuchGiftCode,

    #[error("that gift code has expired")]
    GiftCodeExpired,

    #[error("that gift code has been used up")]
    GiftCodeUsedUp,

    #[error("you've already redeemed that gift code")]
    GiftCodeAlreadyRedeemed,

    #[error("a gift code with that value already exists")]
    GiftCodeExists,

    #[error("that checkout doesn't exist")]
    NoSuchCheckout,

    /// Re-linking two users was refused, with why. Only an admin sees this.
    #[error("can't re-link these users: {0}")]
    CannotRelink(String),

    #[error("project {0} does not exist")]
    NoSuchProject(String),

    #[error("project {0} is disabled")]
    ProjectDisabled(String),

    #[error("project {0} still has buckets")]
    ProjectNotEmpty(String),

    #[error("this user has created as many projects as allowed")]
    ProjectLimitReached,

    #[error("this project has as many access keys as allowed")]
    KeyLimitReached,

    #[error("that user is not a member of this project")]
    NoSuchMember,

    #[error("a project must keep at least one Owner")]
    LastOwner,

    #[error("that invite does not exist")]
    NoSuchInvite,

    #[error("that invite has expired")]
    InviteExpired,

    #[error("that email address already has a pending invite to this project")]
    InviteExists,

    #[error("that person is already a member of this project")]
    AlreadyMember,

    #[error("that invite was sent to a different email address")]
    InviteEmailMismatch,

    #[error("the email address on this account isn't verified")]
    EmailNotVerified,

    #[error("access key {0:?} does not exist")]
    NoSuchCredential(String),

    #[error("owner {0} has no buckets or keys")]
    NoSuchOwner(String),

    #[error("owner {0} already belongs to a project")]
    OwnerAlreadyInProject(String),

    #[error("owner {0} holds an admin key, so it can't become a project")]
    OwnerIsAdmin(String),

    #[error("node {0} is not registered in this cluster")]
    NoSuchNode(String),

    #[error("node {0} was removed from this cluster and cannot rejoin it")]
    NodeRemoved(String),

    #[error("node {node} cannot go from {from} to {to}")]
    InvalidNodeTransition {
        node: String,
        from: String,
        to: String,
    },

    #[error(
        "this node already belongs to cluster {existing:?} and cannot join/bootstrap cluster {requested:?} \
         — refusing to silently merge unrelated clusters"
    )]
    ClusterIdMismatch { existing: String, requested: String },

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("background task panicked: {0}")]
    TaskPanicked(String),

    /// This node's Raft engine couldn't service the request — it isn't the leader (and
    /// doesn't know who is), lost quorum, or the write timed out waiting for consensus
    /// (Phase 8). Distinct from every other variant here, which are business-logic
    /// outcomes a *successful* Raft commit can still produce.
    #[error("metadata Raft group unavailable: {0}")]
    RaftUnavailable(String),
}

impl From<serde_json::Error> for MetaError {
    fn from(e: serde_json::Error) -> Self {
        MetaError::Serialization(e.to_string())
    }
}
