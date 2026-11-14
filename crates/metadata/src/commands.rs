//! Command/query/response types for [`crate::MetadataStore`]. These are shaped so a
//! future Raft integration (Phase 8) can serialize the mutating ones as log entries
//! directly — see architecture.md §5/§26 for why Phase 2 doesn't wire real `openraft`
//! yet: with exactly one voter, a replicated log and a plain WAL are the same thing.

use std::collections::{BTreeMap, HashMap};

use loony_core::{
    BucketId, BucketName, ETag, InviteId, NodeId, ObjectKey, OwnerId, PartManifest, ProjectId,
    UploadId, UserId, VolumeId,
};
use time::OffsetDateTime;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateBucket {
    pub name: BucketName,
    pub owner_id: OwnerId,
    pub region: String,
}

#[derive(Debug, Clone)]
pub struct ListObjectsQuery {
    pub bucket_id: BucketId,
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    pub start_after: Option<String>,
    pub continuation_token: Option<String>,
    pub max_keys: u32,
}

#[derive(Debug, Clone)]
pub struct ObjectSummary {
    pub key: ObjectKey,
    pub size: u64,
    pub etag: ETag,
    pub last_modified: OffsetDateTime,
}

#[derive(Debug, Clone, Default)]
pub struct ListObjectsPage {
    pub objects: Vec<ObjectSummary>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BeginMultipart {
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultipartUploadState {
    pub upload_id: UploadId,
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
    #[serde(with = "time::serde::rfc3339")]
    pub initiated_at: OffsetDateTime,
    pub parts: BTreeMap<u32, PartManifest>,
}

#[derive(Debug, Clone)]
pub struct PartSummary {
    pub part_number: u32,
    pub size: u64,
    pub etag_md5: [u8; 16],
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompleteMultipart {
    pub upload_id: UploadId,
    /// Client-supplied ordered `(part_number, etag_hex)` list, validated against the
    /// parts actually recorded via `record_part` (prompt §57: `InvalidPart` /
    /// `InvalidPartOrder`).
    pub requested_parts: Vec<(u32, String)>,
    pub content_type: String,
}

/// Registers (or re-registers, bumping `generation`) a node in the cluster's registry
/// (architecture.md §34).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisterNode {
    pub node_id: NodeId,
    pub advertised_address: String,
    pub failure_domain: Vec<String>,
    pub volumes: Vec<VolumeId>,
}

/// A credential record (architecture.md §54). `secret_key` is stored as configured
/// (encrypted at rest once a KMS/envelope key is wired up — not yet in this phase),
/// never hashed: SigV4 verification needs to recompute an HMAC from the actual secret,
/// which a one-way hash can't support (architecture.md §24).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Credential {
    pub access_key: String,
    pub secret_key: String,
    pub owner_id: OwnerId,
    pub enabled: bool,
    /// Whether this key may call the admin API. Only the configured root credential is
    /// ever seeded with this set; keys created through the admin API never are. Defaults
    /// to `false` so records written before this field existed stay non-admin until the
    /// root credential is re-seeded at startup.
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub kind: CredentialKind,
    /// The signed-in user this key was issued to. `None` for keys an admin created.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// When this key stops working. `None` means it doesn't expire.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// The project this key acts for. Its `owner_id` is then the project's.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    /// What people call this key, for keys they manage themselves.
    #[serde(default)]
    pub name: Option<String>,
    /// For a project session key, the user session it was opened from. Signing out of that
    /// session ends this key too.
    #[serde(default)]
    pub session_of: Option<String>,
    /// Roughly when this key was last used: see `MetadataStore::touch_credential`.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl Credential {
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }

    /// Whether this is one of the short-lived keys behind a web console sign-in.
    pub fn is_session(&self) -> bool {
        matches!(
            self.kind,
            CredentialKind::UserSession | CredentialKind::ProjectSession
        )
    }
}

/// What a key is allowed to be used for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A normal key, for the LS3 API. Every key written before this field existed.
    #[default]
    Standard,
    /// Proves who is signed in to the web console. Refused by the LS3 API, so a login
    /// session can never own or touch buckets by itself.
    UserSession,
    /// What the web console uses to work inside one project, on behalf of one member.
    ProjectSession,
    /// A project's long-lived key, for its apps. Belongs to the project rather than the
    /// member who created it, so it keeps working after they leave.
    ProjectAccessKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRole {
    /// Can do everything, including managing members and deleting the project.
    Owner,
    /// Can manage buckets and access keys.
    Member,
}

/// Buckets belong to `owner_id`, so LS3's ownership checks keep projects apart.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    pub project_id: ProjectId,
    pub name: String,
    pub owner_id: OwnerId,
    /// Whether an admin has blocked this project. All its keys stop working while it's off.
    pub enabled: bool,
    pub created_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectMember {
    pub project_id: ProjectId,
    pub user_id: UserId,
    pub role: ProjectRole,
    #[serde(with = "time::serde::rfc3339")]
    pub added_at: OffsetDateTime,
}

/// A pending invitation. It's deleted once answered or cancelled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectInvite {
    pub invite_id: InviteId,
    pub project_id: ProjectId,
    /// Lowercase.
    pub email: String,
    pub role: ProjectRole,
    pub invited_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateProject {
    pub name: String,
    pub created_by: UserId,
    /// How many projects `created_by` may have created, counting this one.
    pub max_created: u32,
}

/// Turns an owner from before projects existed into a project, with its buckets and keys.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdoptOwner {
    pub owner_id: OwnerId,
    pub name: String,
    /// Who becomes the project's Owner.
    pub user_id: UserId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateInvite {
    pub project_id: ProjectId,
    /// Lowercase.
    pub email: String,
    pub role: ProjectRole,
    pub invited_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Someone who has signed in through the identity provider. The account itself, with its
/// password, lives in the identity provider: this holds what LS3 needs to know about them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct User {
    pub user_id: UserId,
    /// Who vouches for this user: `issuer` and `subject` together are the stable identity,
    /// since an email address can change.
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    /// Whether an admin has blocked this user from LS3.
    pub enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_sign_in_at: OffsetDateTime,
}

/// What the identity provider said about someone who just signed in.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserSignIn {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
}

/// What an admin controls about a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserAccess {
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub object_count: u64,
    pub total_bytes: u64,
}

/// Cluster-wide usage, computed by scanning every manifest and in-progress upload, so
/// its cost grows with the object count. For occasional operator views, not hot paths.
#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub buckets: HashMap<BucketId, BucketUsage>,
    /// How many shards each node holds, counting committed objects and in-progress
    /// multipart parts: a node with a non-zero count can't be safely removed.
    pub shards_by_node: HashMap<NodeId, u64>,
    pub multipart_uploads: u64,
}
