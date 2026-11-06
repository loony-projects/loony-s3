//! The `MetadataStore` trait (architecture.md §4/§37). The authoritative, transactional
//! source of truth for buckets, objects/manifests, and multipart uploads. `cluster`
//! and node/credential-registry methods are added in later phases as those subsystems
//! land; this phase covers what Phase 3 (Standalone LS3) needs.

use async_trait::async_trait;
use loony_core::{
    Bucket, BucketId, BucketName, ClusterId, NodeId, NodeInfo, NodeState, ObjectKey,
    ObjectManifest, OwnerId, UploadId, UserId, InviteId, ProjectId,
};

use crate::commands::{
    AdoptOwner, BeginMultipart, CompleteMultipart, CreateBucket, CreateInvite, CreateProject,
    Credential, ListObjectsPage, ListObjectsQuery, MultipartUploadState, PartSummary, Project,
    ProjectInvite, ProjectMember, ProjectRole, RegisterNode, StorageStats, User, UserAccess,
    UserSignIn,
};
use crate::error::MetaError;
use loony_core::PartManifest;

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_bucket(&self, cmd: CreateBucket) -> Result<Bucket, MetaError>;
    async fn delete_bucket(&self, name: &BucketName) -> Result<(), MetaError>;
    async fn get_bucket(&self, name: &BucketName) -> Result<Option<Bucket>, MetaError>;
    async fn list_buckets(&self, owner: OwnerId) -> Result<Vec<Bucket>, MetaError>;
    /// Every bucket regardless of owner. For the admin API only: LS3 requests are always
    /// scoped to the caller's own buckets through [`Self::list_buckets`].
    async fn list_all_buckets(&self) -> Result<Vec<Bucket>, MetaError>;
    async fn storage_stats(&self) -> Result<StorageStats, MetaError>;

    /// Atomically make `manifest` the current version of its `(bucket_id, key)` — the
    /// visibility point every PUT/CompleteMultipart converges on (architecture.md §21).
    async fn commit_manifest(&self, manifest: ObjectManifest) -> Result<ObjectManifest, MetaError>;
    async fn get_manifest(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectManifest>, MetaError>;
    /// Idempotent: tombstoning an already-absent object is not an error.
    async fn tombstone_object(&self, bucket_id: BucketId, key: &ObjectKey)
    -> Result<(), MetaError>;
    async fn list_objects(&self, query: ListObjectsQuery) -> Result<ListObjectsPage, MetaError>;

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError>;
    /// The upload's own record -- bucket/key/content-type/metadata it was created with
    /// (never the recorded parts; use `list_parts` for those). Callers use this to
    /// authorize an operation against an `upload_id` (confirming it really belongs to
    /// the bucket/key the caller claims) and to recover the content-type/metadata a
    /// `CompleteMultipartUpload` request doesn't itself carry.
    async fn get_upload(
        &self,
        upload_id: UploadId,
    ) -> Result<Option<MultipartUploadState>, MetaError>;
    async fn record_part(&self, upload_id: UploadId, part: PartManifest) -> Result<(), MetaError>;
    async fn list_parts(&self, upload_id: UploadId) -> Result<Vec<PartSummary>, MetaError>;
    async fn complete_multipart(&self, cmd: CompleteMultipart)
    -> Result<ObjectManifest, MetaError>;
    /// Idempotent: aborting an already-aborted/completed/unknown upload is not an error.
    async fn abort_multipart(&self, upload_id: UploadId) -> Result<(), MetaError>;

    async fn put_credential(&self, cred: Credential) -> Result<(), MetaError>;
    async fn get_credential(&self, access_key: &str) -> Result<Option<Credential>, MetaError>;
    async fn list_credentials(&self) -> Result<Vec<Credential>, MetaError>;
    /// Idempotent: deleting an unknown access key is not an error.
    async fn delete_credential(&self, access_key: &str) -> Result<(), MetaError>;

    /// Records a sign-in: creates the user the first time this issuer and subject are
    /// seen, and otherwise refreshes what the identity provider said about them, leaving
    /// what an admin controls ([`UserAccess`]) alone.
    async fn record_user_sign_in(&self, sign_in: UserSignIn) -> Result<User, MetaError>;
    async fn get_user(&self, user_id: UserId) -> Result<Option<User>, MetaError>;
    async fn list_users(&self) -> Result<Vec<User>, MetaError>;
    /// Disabling a user also ends their project session keys. Project access keys keep
    /// working, because they belong to the project.
    async fn set_user_access(&self, user_id: UserId, access: UserAccess)
    -> Result<User, MetaError>;

    /// Creates the project with `created_by` as its Owner. Fails with
    /// [`MetaError::ProjectLimitReached`] past `max_created`.
    async fn create_project(&self, cmd: CreateProject) -> Result<Project, MetaError>;
    async fn adopt_owner(&self, cmd: AdoptOwner) -> Result<Project, MetaError>;
    async fn get_project(&self, project_id: ProjectId) -> Result<Option<Project>, MetaError>;
    async fn list_projects(&self) -> Result<Vec<Project>, MetaError>;
    async fn rename_project(&self, project_id: ProjectId, name: String)
    -> Result<Project, MetaError>;
    /// Disabling a project stops all its keys at once, and ends its session keys for good.
    async fn set_project_enabled(
        &self,
        project_id: ProjectId,
        enabled: bool,
    ) -> Result<Project, MetaError>;
    /// Removes the project with its keys, members and invites. Fails with
    /// [`MetaError::ProjectNotEmpty`] while it still has buckets.
    async fn delete_project(&self, project_id: ProjectId) -> Result<(), MetaError>;

    async fn get_member(
        &self,
        project_id: ProjectId,
        user_id: UserId,
    ) -> Result<Option<ProjectMember>, MetaError>;
    async fn list_members(&self, project_id: ProjectId) -> Result<Vec<ProjectMember>, MetaError>;
    async fn list_user_memberships(&self, user_id: UserId)
    -> Result<Vec<ProjectMember>, MetaError>;
    /// Fails with [`MetaError::LastOwner`] rather than leave a project without an Owner.
    async fn set_member_role(
        &self,
        project_id: ProjectId,
        user_id: UserId,
        role: ProjectRole,
    ) -> Result<ProjectMember, MetaError>;
    /// Also ends their session keys for this project. Fails with [`MetaError::LastOwner`]
    /// rather than leave a project without an Owner.
    async fn remove_member(&self, project_id: ProjectId, user_id: UserId) -> Result<(), MetaError>;

    /// Fails with [`MetaError::InviteExists`] or [`MetaError::AlreadyMember`] if there's
    /// nothing to invite them to.
    async fn create_invite(&self, cmd: CreateInvite) -> Result<ProjectInvite, MetaError>;
    async fn get_invite(&self, invite_id: InviteId) -> Result<Option<ProjectInvite>, MetaError>;
    async fn list_project_invites(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectInvite>, MetaError>;
    async fn list_invites(&self) -> Result<Vec<ProjectInvite>, MetaError>;
    async fn delete_invite(&self, invite_id: InviteId) -> Result<(), MetaError>;
    /// Only for the user whose verified email the invite was sent to.
    async fn accept_invite(
        &self,
        invite_id: InviteId,
        user_id: UserId,
    ) -> Result<ProjectMember, MetaError>;
    /// Only for the user whose verified email the invite was sent to.
    async fn decline_invite(&self, invite_id: InviteId, user_id: UserId) -> Result<(), MetaError>;

    /// Stores a key for `credential.project_id`, checking in the same step that the project
    /// is enabled and, for a session key, that its user is still a member, so a key can't
    /// slip through just as someone is removed. Fails with [`MetaError::KeyLimitReached`]
    /// past `max_access_keys`.
    async fn put_project_credential(
        &self,
        credential: Credential,
        max_access_keys: u32,
    ) -> Result<(), MetaError>;
    async fn set_credential_enabled(
        &self,
        access_key: &str,
        enabled: bool,
    ) -> Result<(), MetaError>;
    /// Records that a key was just used. Does nothing if it's since been deleted.
    async fn touch_credential(
        &self,
        access_key: &str,
        at: time::OffsetDateTime,
    ) -> Result<(), MetaError>;
    /// Ends a user session and every project session key opened from it.
    async fn delete_session(&self, access_key: &str) -> Result<(), MetaError>;

    /// Sets this store's cluster identity if it doesn't have one yet; idempotent (and
    /// safe to call again with the same id) once it does. Errors with
    /// [`MetaError::ClusterIdMismatch`] rather than silently adopting a different
    /// cluster (architecture.md §36).
    async fn bootstrap_cluster(&self, cluster_id: ClusterId) -> Result<(), MetaError>;
    async fn get_cluster_id(&self) -> Result<Option<ClusterId>, MetaError>;

    /// Registers `node_id`, bumping `generation` if it was already registered
    /// (architecture.md §34). A draining node stays draining across a re-registration,
    /// and a removed node is refused with [`MetaError::NodeRemoved`].
    async fn register_node(&self, cmd: RegisterNode) -> Result<NodeInfo, MetaError>;
    /// Fails with [`MetaError::InvalidNodeTransition`] if the node's current state doesn't
    /// allow it: see `check_node_transition`.
    async fn update_node_state(&self, node_id: NodeId, state: NodeState) -> Result<(), MetaError>;
    async fn list_nodes(&self) -> Result<Vec<NodeInfo>, MetaError>;
}
