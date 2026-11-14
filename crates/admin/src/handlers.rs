use std::collections::{BTreeMap, HashMap, HashSet};

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use loony_core::{BucketName, GiftCodeId, NodeId, NodeState, OwnerId, ProjectId, UserId};
use loony_metadata::billing::gift_code_hash;
use loony_metadata::{
    AdoptOwner, Credential, CredentialKind, GiftCode, ListObjectsQuery, MetadataStore, NewGiftCode,
    Project, ProjectRole, SubscriptionState, User, UserAccess,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::AdminState;
use crate::audit::AuditEntry;
use crate::auth::Actor;
use crate::error::AdminError;

type Result<T> = std::result::Result<T, AdminError>;

const OBJECT_PAGE_SIZE: u32 = 200;
const DEFAULT_AUDIT_LIMIT: usize = 200;
const MAX_AUDIT_LIMIT: usize = 1000;

/// Records the outcome of an admin action, whichever way it went.
async fn audited<T>(
    state: &AdminState,
    actor: &Actor,
    action: &str,
    target: &str,
    result: Result<T>,
) -> Result<T> {
    let (outcome, detail) = match &result {
        Ok(_) => ("ok", None),
        // A refused action changed nothing; a failed one may be worth investigating.
        Err(err) if err.status.is_client_error() => ("denied", Some(err.message.clone())),
        Err(err) => ("failed", Some(err.message.clone())),
    };
    state
        .audit
        .record(AuditEntry {
            at: OffsetDateTime::now_utc(),
            actor: actor.0.clone(),
            action: action.to_string(),
            target: target.to_string(),
            outcome: outcome.to_string(),
            detail,
        })
        .await;
    result
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Overview {
    mode: &'static str,
    node_id: NodeId,
    cluster_id: Option<String>,
    region: String,
    version: &'static str,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    uptime_seconds: i64,
    metadata: MetadataStatus,
    totals: Totals,
    nodes_by_state: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct MetadataStatus {
    state: String,
    term: u64,
    leader: Option<NodeId>,
    voters: usize,
    learners: usize,
}

#[derive(Serialize)]
struct Totals {
    buckets: u64,
    objects: u64,
    bytes: u64,
    owners: u64,
    credentials: u64,
    enabled_credentials: u64,
    multipart_uploads: u64,
    nodes: u64,
}

pub async fn overview(State(state): State<AdminState>) -> Result<Json<Overview>> {
    let buckets = state.metadata.list_all_buckets().await?;
    let credentials = standard_credentials(&state).await?;
    let stats = state.metadata.storage_stats().await?;
    let nodes = state.metadata.list_nodes().await?;
    let raft = state.metadata.raft_status();

    let owners: HashSet<OwnerId> = buckets
        .iter()
        .map(|b| b.owner_id)
        .chain(credentials.iter().map(|c| c.owner_id))
        .collect();
    let mut nodes_by_state = BTreeMap::new();
    for node in &nodes {
        *nodes_by_state
            .entry(format!("{:?}", node.state))
            .or_default() += 1;
    }

    Ok(Json(Overview {
        mode: state.mode,
        node_id: state.node_id,
        cluster_id: state
            .metadata
            .get_cluster_id()
            .await?
            .map(|id| id.to_string()),
        region: state.region.clone(),
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        uptime_seconds: (OffsetDateTime::now_utc() - state.started_at).whole_seconds(),
        metadata: MetadataStatus {
            state: raft.server_state,
            term: raft.current_term,
            leader: raft.leader,
            voters: raft.voters.len(),
            learners: raft.learners.len(),
        },
        totals: Totals {
            buckets: buckets.len() as u64,
            objects: stats.buckets.values().map(|u| u.object_count).sum(),
            bytes: stats.buckets.values().map(|u| u.total_bytes).sum(),
            owners: owners.len() as u64,
            credentials: credentials.len() as u64,
            enabled_credentials: credentials.iter().filter(|c| c.enabled).count() as u64,
            multipart_uploads: stats.multipart_uploads,
            nodes: nodes.len() as u64,
        },
        nodes_by_state,
    }))
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct NodeView {
    node_id: NodeId,
    advertised_address: String,
    state: NodeState,
    generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    last_seen: OffsetDateTime,
    volumes: usize,
    shards: u64,
    /// The node answering this request.
    is_self: bool,
    is_leader: bool,
    is_voter: bool,
}

pub async fn list_nodes(State(state): State<AdminState>) -> Result<Json<Vec<NodeView>>> {
    let nodes = state.metadata.list_nodes().await?;
    let stats = state.metadata.storage_stats().await?;
    let raft = state.metadata.raft_status();
    Ok(Json(
        nodes
            .into_iter()
            .map(|node| NodeView {
                shards: stats
                    .shards_by_node
                    .get(&node.node_id)
                    .copied()
                    .unwrap_or(0),
                is_self: node.node_id == state.node_id,
                is_leader: raft.leader == Some(node.node_id),
                is_voter: raft.voters.contains(&node.node_id),
                node_id: node.node_id,
                advertised_address: node.advertised_address,
                state: node.state,
                generation: node.generation,
                last_seen: node.last_seen,
                volumes: node.volumes.len(),
            })
            .collect(),
    ))
}

async fn node_state(state: &AdminState, node_id: NodeId) -> Result<NodeState> {
    state
        .metadata
        .list_nodes()
        .await?
        .into_iter()
        .find(|n| n.node_id == node_id)
        .map(|n| n.state)
        .ok_or_else(|| AdminError::not_found(format!("Node {node_id} is not registered")))
}

/// Voters (including the leader) keep the metadata group alive: taking one out needs a
/// voter membership change this project doesn't implement yet.
fn refuse_if_voter(state: &AdminState, node_id: NodeId) -> Result<()> {
    if state.metadata.raft_status().voters.contains(&node_id) {
        return Err(AdminError::conflict(
            "This node is a metadata voter, and the cluster depends on it. Voter changes aren't \
             supported yet.",
        ));
    }
    Ok(())
}

pub async fn drain_node(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(node_id): Path<NodeId>,
) -> Result<StatusCode> {
    let result = async {
        node_state(&state, node_id).await?;
        refuse_if_voter(&state, node_id)?;
        state
            .metadata
            .update_node_state(node_id, NodeState::Draining)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    audited(&state, &actor, "node.drain", &node_id.to_string(), result).await
}

pub async fn resume_node(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(node_id): Path<NodeId>,
) -> Result<StatusCode> {
    let result = async {
        if node_state(&state, node_id).await? != NodeState::Draining {
            return Err(AdminError::conflict("Only a draining node can be resumed"));
        }
        // Heartbeats promote it back to Healthy once it answers.
        state
            .metadata
            .update_node_state(node_id, NodeState::Joining)
            .await?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    audited(&state, &actor, "node.resume", &node_id.to_string(), result).await
}

pub async fn remove_node(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(node_id): Path<NodeId>,
) -> Result<StatusCode> {
    let result = async {
        let current = node_state(&state, node_id).await?;
        // `Removed` is allowed through so a failed membership change below can be retried.
        if !matches!(current, NodeState::Draining | NodeState::Removed) {
            return Err(AdminError::conflict("Drain this node before removing it"));
        }
        refuse_if_voter(&state, node_id)?;

        // Nothing moves shards off a node yet, so removing one that still holds any would
        // make those objects unreadable.
        let shards = state
            .metadata
            .storage_stats()
            .await?
            .shards_by_node
            .get(&node_id)
            .copied()
            .unwrap_or(0);
        if shards > 0 {
            return Err(AdminError::conflict(format!(
                "This node still holds {shards} shard(s), and shards can't be migrated yet. \
                 Removing it would make that data unreadable."
            )));
        }

        state
            .metadata
            .update_node_state(node_id, NodeState::Removed)
            .await?;
        state.metadata.remove_learner(node_id).await?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    audited(&state, &actor, "node.remove", &node_id.to_string(), result).await
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct BucketView {
    name: String,
    owner_id: OwnerId,
    region: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    object_count: u64,
    total_bytes: u64,
}

pub async fn list_buckets(State(state): State<AdminState>) -> Result<Json<Vec<BucketView>>> {
    let buckets = state.metadata.list_all_buckets().await?;
    let stats = state.metadata.storage_stats().await?;
    Ok(Json(
        buckets
            .into_iter()
            .map(|bucket| {
                let usage = stats
                    .buckets
                    .get(&bucket.bucket_id)
                    .copied()
                    .unwrap_or_default();
                BucketView {
                    name: bucket.name.as_str().to_string(),
                    owner_id: bucket.owner_id,
                    region: bucket.region,
                    created_at: bucket.created_at,
                    object_count: usage.object_count,
                    total_bytes: usage.total_bytes,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct ListObjectsParams {
    prefix: Option<String>,
    cursor: Option<String>,
}

#[derive(Serialize)]
pub struct ObjectPage {
    folders: Vec<String>,
    objects: Vec<ObjectView>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct ObjectView {
    key: String,
    size: u64,
    etag: String,
    #[serde(with = "time::serde::rfc3339")]
    last_modified: OffsetDateTime,
}

/// Read-only on purpose: an admin can see what's in anyone's bucket (names and sizes,
/// not contents), and each look is recorded.
pub async fn list_objects(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(name): Path<String>,
    Query(params): Query<ListObjectsParams>,
) -> Result<Json<ObjectPage>> {
    let result = async {
        let no_such_bucket = || AdminError::not_found(format!("Bucket {name:?} does not exist"));
        let bucket_name = BucketName::parse(name.as_str()).map_err(|_| no_such_bucket())?;
        let bucket = state
            .metadata
            .get_bucket(&bucket_name)
            .await?
            .ok_or_else(no_such_bucket)?;
        let page = state
            .metadata
            .list_objects(ListObjectsQuery {
                bucket_id: bucket.bucket_id,
                prefix: params.prefix.clone().filter(|p| !p.is_empty()),
                delimiter: Some("/".to_string()),
                start_after: None,
                continuation_token: params.cursor.clone().filter(|c| !c.is_empty()),
                max_keys: OBJECT_PAGE_SIZE,
            })
            .await?;
        Ok(Json(ObjectPage {
            folders: page.common_prefixes,
            objects: page
                .objects
                .into_iter()
                .map(|o| ObjectView {
                    key: o.key.to_string(),
                    size: o.size,
                    etag: o.etag.to_string(),
                    last_modified: o.last_modified,
                })
                .collect(),
            next_cursor: page.next_continuation_token,
        }))
    }
    .await;
    let target = format!("{name}/{}", params.prefix.as_deref().unwrap_or(""));
    audited(&state, &actor, "bucket.browse", &target, result).await
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// A credential as the API shows it. Has no secret field: once created, a secret can't
/// be read back through this API.
#[derive(Serialize)]
pub struct CredentialView {
    access_key: String,
    owner_id: OwnerId,
    enabled: bool,
    admin: bool,
    project_id: Option<ProjectId>,
    name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl From<Credential> for CredentialView {
    fn from(cred: Credential) -> Self {
        Self {
            access_key: cred.access_key,
            owner_id: cred.owner_id,
            enabled: cred.enabled,
            admin: cred.admin,
            project_id: cred.project_id,
            name: cred.name,
            created_at: cred.created_at,
        }
    }
}

#[derive(Serialize)]
pub struct UserView {
    id: UserId,
    email: Option<String>,
    email_verified: bool,
    name: Option<String>,
    enabled: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_sign_in_at: OffsetDateTime,
}

impl From<User> for UserView {
    fn from(user: User) -> Self {
        Self {
            id: user.user_id,
            email: user.email,
            email_verified: user.email_verified,
            name: user.name,
            enabled: user.enabled,
            created_at: user.created_at,
            last_sign_in_at: user.last_sign_in_at,
        }
    }
}

pub async fn list_users(State(state): State<AdminState>) -> Result<Json<Vec<UserView>>> {
    let mut users = state.metadata.list_users().await?;
    users.sort_by_key(|user| std::cmp::Reverse(user.last_sign_in_at));
    Ok(Json(users.into_iter().map(Into::into).collect()))
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    enabled: Option<bool>,
}

/// Whatever isn't in the request stays as it is.
pub async fn update_user(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(user_id): Path<UserId>,
    Json(request): Json<UpdateUserRequest>,
) -> Result<Json<UserView>> {
    let target = format!("user:{user_id}");
    let result = async {
        let user = state
            .metadata
            .get_user(user_id)
            .await?
            .ok_or_else(|| AdminError::not_found(format!("User {user_id} does not exist")))?;
        let access = UserAccess {
            enabled: request.enabled.unwrap_or(user.enabled),
        };
        Ok(Json(
            state
                .metadata
                .set_user_access(user_id, access)
                .await?
                .into(),
        ))
    }
    .await;
    audited(&state, &actor, "user.update", &target, result).await
}

#[derive(Serialize)]
pub struct ProjectMemberView {
    user_id: UserId,
    email: Option<String>,
    name: Option<String>,
    role: ProjectRole,
}

#[derive(Serialize)]
pub struct ProjectView {
    id: ProjectId,
    name: String,
    enabled: bool,
    owner_id: OwnerId,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    members: Vec<ProjectMemberView>,
    access_keys: u64,
    buckets: u64,
    objects: u64,
    bytes: u64,
    subscription: SubscriptionState,
    plan_name: Option<String>,
    /// The plan's storage. Whether anything can be stored depends on `subscription`.
    limit_bytes: Option<u64>,
    #[serde(with = "time::serde::rfc3339::option")]
    period_end: Option<OffsetDateTime>,
    cancel_at_period_end: bool,
}

/// How much is stored for each owner, in buckets, objects and bytes.
async fn usage_by_owner(state: &AdminState) -> Result<HashMap<OwnerId, (u64, u64, u64)>> {
    let stats = state.metadata.storage_stats().await?;
    let mut usage: HashMap<OwnerId, (u64, u64, u64)> = HashMap::new();
    for bucket in state.metadata.list_all_buckets().await? {
        let bucket_usage = stats
            .buckets
            .get(&bucket.bucket_id)
            .copied()
            .unwrap_or_default();
        let entry = usage.entry(bucket.owner_id).or_default();
        entry.0 += 1;
        entry.1 += bucket_usage.object_count;
        entry.2 += bucket_usage.total_bytes;
    }
    Ok(usage)
}

async fn project_view(
    state: &AdminState,
    project: Project,
    usage: &HashMap<OwnerId, (u64, u64, u64)>,
    credentials: &[Credential],
) -> Result<ProjectView> {
    let mut members = Vec::new();
    for member in state.metadata.list_members(project.project_id).await? {
        let user = state.metadata.get_user(member.user_id).await?;
        members.push(ProjectMemberView {
            user_id: member.user_id,
            email: user.as_ref().and_then(|u| u.email.clone()),
            name: user.and_then(|u| u.name),
            role: member.role,
        });
    }
    let (buckets, objects, bytes) = usage.get(&project.owner_id).copied().unwrap_or_default();
    let access_keys = credentials
        .iter()
        .filter(|c| {
            c.kind == CredentialKind::ProjectAccessKey && c.project_id == Some(project.project_id)
        })
        .count() as u64;
    let sub = state.metadata.get_subscription(project.project_id).await?;
    Ok(ProjectView {
        subscription: sub.as_ref().map_or(SubscriptionState::Unpaid, |s| s.state),
        plan_name: sub.as_ref().map(|s| s.plan.name.clone()),
        limit_bytes: sub.as_ref().map(|s| s.plan.storage_bytes),
        period_end: sub.as_ref().map(|s| s.period_end),
        cancel_at_period_end: sub.as_ref().is_some_and(|s| s.cancel_at_period_end),
        id: project.project_id,
        name: project.name,
        enabled: project.enabled,
        owner_id: project.owner_id,
        created_at: project.created_at,
        members,
        access_keys,
        buckets,
        objects,
        bytes,
    })
}

pub async fn list_projects(State(state): State<AdminState>) -> Result<Json<Vec<ProjectView>>> {
    let mut projects = state.metadata.list_projects().await?;
    projects.sort_by_key(|project| std::cmp::Reverse(project.created_at));
    let usage = usage_by_owner(&state).await?;
    let credentials = state.metadata.list_credentials().await?;
    let mut views = Vec::with_capacity(projects.len());
    for project in projects {
        views.push(project_view(&state, project, &usage, &credentials).await?);
    }
    Ok(Json(views))
}

#[derive(Deserialize)]
pub struct UpdateProjectRequest {
    enabled: bool,
}

pub async fn update_project(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(project_id): Path<ProjectId>,
    Json(request): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectView>> {
    let action = if request.enabled {
        "project.enable"
    } else {
        "project.disable"
    };
    let target = format!("project:{project_id}");
    let result = async {
        let project = state
            .metadata
            .set_project_enabled(project_id, request.enabled)
            .await?;
        let usage = usage_by_owner(&state).await?;
        let credentials = state.metadata.list_credentials().await?;
        Ok(Json(
            project_view(&state, project, &usage, &credentials).await?,
        ))
    }
    .await;
    audited(&state, &actor, action, &target, result).await
}

#[derive(Serialize)]
pub struct AdoptableOwner {
    owner_id: OwnerId,
    access_keys: u64,
    buckets: u64,
    objects: u64,
    bytes: u64,
}

/// Owners from before projects existed. Leaves out anyone holding an admin key, which can't
/// become a project.
pub async fn adoptable_owners(
    State(state): State<AdminState>,
) -> Result<Json<Vec<AdoptableOwner>>> {
    let usage = usage_by_owner(&state).await?;
    let in_project: HashSet<OwnerId> = state
        .metadata
        .list_projects()
        .await?
        .into_iter()
        .map(|project| project.owner_id)
        .collect();

    let mut keys: HashMap<OwnerId, u64> = HashMap::new();
    let mut admins = HashSet::new();
    for credential in state.metadata.list_credentials().await? {
        if credential.kind == CredentialKind::Standard {
            *keys.entry(credential.owner_id).or_default() += 1;
            if credential.admin {
                admins.insert(credential.owner_id);
            }
        }
    }

    let owners: HashSet<OwnerId> = keys.keys().chain(usage.keys()).copied().collect();
    let mut adoptable: Vec<AdoptableOwner> = owners
        .into_iter()
        .filter(|owner| !in_project.contains(owner) && !admins.contains(owner))
        .map(|owner| {
            let (buckets, objects, bytes) = usage.get(&owner).copied().unwrap_or_default();
            AdoptableOwner {
                owner_id: owner,
                access_keys: keys.get(&owner).copied().unwrap_or_default(),
                buckets,
                objects,
                bytes,
            }
        })
        .collect();
    adoptable.sort_by_key(|owner| owner.owner_id);
    Ok(Json(adoptable))
}

// ---------------------------------------------------------------------------
// Gift codes
// ---------------------------------------------------------------------------

/// Up to ₹10,00,000 per code, and up to 1,00,000 uses.
const MAX_GIFT_CODE_PAISE: u64 = 100_000_000;
const MAX_GIFT_CODE_USES: u32 = 100_000;

/// A code drawn from the OS's random number generator, every character equally likely: 17
/// characters from 36 is about 88 bits, far too many to guess.
fn generate_gift_code() -> std::result::Result<String, getrandom::Error> {
    let alphabet = loony_metadata::billing::GIFT_CODE_ALPHABET;
    let mut code = String::with_capacity(loony_metadata::billing::GIFT_CODE_LEN);
    let mut buf = [0u8; 64];
    while code.len() < loony_metadata::billing::GIFT_CODE_LEN {
        getrandom::fill(&mut buf)?;
        for byte in buf {
            // 252 is the largest multiple of 36 below 256: dropping what's above it keeps
            // every character equally likely.
            if byte < 252 && code.len() < loony_metadata::billing::GIFT_CODE_LEN {
                code.push(alphabet[(byte % 36) as usize] as char);
            }
        }
    }
    Ok(code)
}

#[derive(Serialize)]
pub struct RedemptionView {
    user_id: UserId,
    email: Option<String>,
    amount_paise: u64,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct GiftCodeView {
    id: GiftCodeId,
    /// The last characters only: the code itself is shown once, when it's made.
    hint: String,
    amount_paise: u64,
    max_uses: u32,
    uses: u32,
    disabled: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    note: Option<String>,
    created_by: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    redemptions: Vec<RedemptionView>,
}

async fn gift_code_view(state: &AdminState, gift: GiftCode) -> Result<GiftCodeView> {
    let mut redemptions = Vec::new();
    for redemption in state.metadata.list_redemptions(gift.code_id).await? {
        let email = state
            .metadata
            .get_user(redemption.user_id)
            .await?
            .and_then(|u| u.email);
        redemptions.push(RedemptionView {
            user_id: redemption.user_id,
            email,
            amount_paise: redemption.amount_paise,
            at: redemption.at,
        });
    }
    redemptions.sort_by_key(|r| r.at);
    Ok(GiftCodeView {
        id: gift.code_id,
        hint: gift.hint,
        amount_paise: gift.amount_paise,
        max_uses: gift.max_uses,
        uses: gift.uses,
        disabled: gift.disabled,
        expires_at: gift.expires_at,
        note: gift.note,
        created_by: gift.created_by,
        created_at: gift.created_at,
        redemptions,
    })
}

pub async fn list_gift_codes(State(state): State<AdminState>) -> Result<Json<Vec<GiftCodeView>>> {
    let mut codes = state.metadata.list_gift_codes().await?;
    codes.sort_by_key(|code| std::cmp::Reverse(code.created_at));
    let mut views = Vec::with_capacity(codes.len());
    for code in codes {
        views.push(gift_code_view(&state, code).await?);
    }
    Ok(Json(views))
}

#[derive(Deserialize)]
pub struct CreateGiftCodeRequest {
    amount_paise: u64,
    #[serde(default)]
    max_uses: Option<u32>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize)]
pub struct CreatedGiftCode {
    /// Only ever returned here.
    code: String,
    #[serde(flatten)]
    gift: GiftCodeView,
}

pub async fn create_gift_code(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Json(request): Json<CreateGiftCodeRequest>,
) -> Result<(StatusCode, Json<CreatedGiftCode>)> {
    let mut target = "gift_code:new".to_string();
    let result = async {
        let bad = |message: &str| AdminError::new(StatusCode::BAD_REQUEST, message);
        if !(1..=MAX_GIFT_CODE_PAISE).contains(&request.amount_paise) {
            return Err(bad(
                "An amount must be more than zero and at most ₹10,00,000.",
            ));
        }
        let max_uses = request.max_uses.unwrap_or(1);
        if !(1..=MAX_GIFT_CODE_USES).contains(&max_uses) {
            return Err(bad("A code can be used between 1 and 1,00,000 times."));
        }
        if request
            .expires_at
            .is_some_and(|at| at <= OffsetDateTime::now_utc())
        {
            return Err(bad("An expiry must be in the future."));
        }
        let note = request
            .note
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if note
            .as_ref()
            .is_some_and(|n| n.chars().count() > 200 || n.chars().any(char::is_control))
        {
            return Err(bad(
                "A note must be at most 200 characters, without control characters.",
            ));
        }
        let code = generate_gift_code().map_err(|err| {
            tracing::error!(%err, "no randomness for a gift code");
            AdminError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't make a code. Try again.",
            )
        })?;
        let gift = state
            .metadata
            .create_gift_code(NewGiftCode {
                code_hash: gift_code_hash(&code),
                hint: code[code.len() - 4..].to_string(),
                amount_paise: request.amount_paise,
                expires_at: request.expires_at,
                max_uses,
                note,
                created_by: actor.0.clone(),
            })
            .await?;
        target = format!("gift_code:{}", gift.code_id);
        Ok((
            StatusCode::CREATED,
            Json(CreatedGiftCode {
                code,
                gift: gift_code_view(&state, gift).await?,
            }),
        ))
    }
    .await;
    audited(&state, &actor, "gift_code.create", &target.clone(), result).await
}

#[derive(Deserialize)]
pub struct UpdateGiftCodeRequest {
    disabled: bool,
}

pub async fn update_gift_code(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(code_id): Path<GiftCodeId>,
    Json(request): Json<UpdateGiftCodeRequest>,
) -> Result<Json<GiftCodeView>> {
    let action = if request.disabled {
        "gift_code.disable"
    } else {
        "gift_code.enable"
    };
    let result = async {
        let gift = state
            .metadata
            .set_gift_code_disabled(code_id, request.disabled)
            .await?;
        Ok(Json(gift_code_view(&state, gift).await?))
    }
    .await;
    audited(
        &state,
        &actor,
        action,
        &format!("gift_code:{code_id}"),
        result,
    )
    .await
}

#[derive(Deserialize)]
pub struct AdoptOwnerRequest {
    owner_id: OwnerId,
    name: String,
    user_id: UserId,
}

#[derive(Serialize)]
pub struct AdoptedProject {
    project_id: ProjectId,
    name: String,
}

/// Turns an owner from before projects existed into a project, with its buckets and keys,
/// and makes `user_id` its Owner. The root key's owner is refused.
pub async fn adopt_owner(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Json(request): Json<AdoptOwnerRequest>,
) -> Result<(StatusCode, Json<AdoptedProject>)> {
    let target = request.owner_id.to_string();
    let result = async {
        let name = request.name.trim();
        if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(AdminError::new(
                StatusCode::BAD_REQUEST,
                "A project name must be 1 to 100 characters, without control characters.",
            ));
        }
        let project = state
            .metadata
            .adopt_owner(AdoptOwner {
                owner_id: request.owner_id,
                name: name.to_string(),
                user_id: request.user_id,
            })
            .await?;
        Ok((
            StatusCode::CREATED,
            Json(AdoptedProject {
                project_id: project.project_id,
                name: project.name,
            }),
        ))
    }
    .await;
    audited(&state, &actor, "project.adopt_owner", &target, result).await
}

/// Leaves out the short-lived keys behind web console sign-ins: they come and go on their
/// own, and an operator has no use for them in a list of keys.
async fn standard_credentials(state: &AdminState) -> Result<Vec<Credential>> {
    let mut credentials = state.metadata.list_credentials().await?;
    credentials.retain(|c| !c.is_session());
    Ok(credentials)
}

pub async fn list_credentials(
    State(state): State<AdminState>,
) -> Result<Json<Vec<CredentialView>>> {
    let mut credentials = standard_credentials(&state).await?;
    credentials.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    Ok(Json(credentials.into_iter().map(Into::into).collect()))
}

#[derive(Deserialize)]
pub struct CreateCredentialRequest {
    /// Adds a key to an existing owner; omit to create a new, empty owner.
    owner_id: Option<OwnerId>,
}

#[derive(Serialize)]
pub struct CreatedCredential {
    #[serde(flatten)]
    credential: CredentialView,
    /// Only ever returned here, in the response that creates it.
    secret_key: String,
}

/// `Uuid::new_v4` draws from the OS CSPRNG, for 122 random bits per id.
fn random_hex(ids: usize) -> String {
    (0..ids)
        .map(|_| uuid::Uuid::new_v4().simple().to_string())
        .collect()
}

pub async fn create_credential(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Json(request): Json<CreateCredentialRequest>,
) -> Result<(StatusCode, Json<CreatedCredential>)> {
    let access_key = format!("LS3{}", random_hex(1)[..17].to_uppercase());
    let result = async {
        let owner_id = match request.owner_id {
            Some(owner_id) => {
                let has_key = state
                    .metadata
                    .list_credentials()
                    .await?
                    .iter()
                    .any(|c| c.owner_id == owner_id);
                let has_bucket = !state.metadata.list_buckets(owner_id).await?.is_empty();
                if !has_key && !has_bucket {
                    return Err(AdminError::not_found(format!(
                        "Owner {owner_id} does not exist"
                    )));
                }
                // A key made here wouldn't belong to the project, so disabling the project
                // wouldn't stop it.
                let in_project = state
                    .metadata
                    .list_projects()
                    .await?
                    .iter()
                    .any(|p| p.owner_id == owner_id);
                if in_project {
                    return Err(AdminError::conflict(
                        "This owner belongs to a project, so its keys are managed there.",
                    ));
                }
                owner_id
            }
            None => OwnerId::new(),
        };
        let credential = Credential {
            access_key: access_key.clone(),
            secret_key: random_hex(2),
            owner_id,
            enabled: true,
            admin: false,
            kind: CredentialKind::Standard,
            user_id: None,
            expires_at: None,
            project_id: None,
            name: None,
            session_of: None,
            last_used_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        state.metadata.put_credential(credential.clone()).await?;
        let secret_key = credential.secret_key.clone();
        Ok((
            StatusCode::CREATED,
            Json(CreatedCredential {
                credential: credential.into(),
                secret_key,
            }),
        ))
    }
    .await;
    audited(&state, &actor, "credential.create", &access_key, result).await
}

async fn existing_credential(state: &AdminState, access_key: &str) -> Result<Credential> {
    state
        .metadata
        .get_credential(access_key)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("Access key {access_key} does not exist")))
}

/// Since the caller is itself an enabled admin key, refusing changes to it also means
/// these operations can never lock every admin out.
fn refuse_if_own_key(actor: &Actor, access_key: &str) -> Result<()> {
    if actor.0 == access_key {
        return Err(AdminError::conflict(
            "You can't disable or delete the key you're signed in with",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct SetEnabledRequest {
    enabled: bool,
}

pub async fn set_credential_enabled(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(access_key): Path<String>,
    Json(request): Json<SetEnabledRequest>,
) -> Result<Json<CredentialView>> {
    let result = async {
        refuse_if_own_key(&actor, &access_key)?;
        let mut credential = existing_credential(&state, &access_key).await?;
        credential.enabled = request.enabled;
        state.metadata.put_credential(credential.clone()).await?;
        Ok(Json(credential.into()))
    }
    .await;
    let action = if request.enabled {
        "credential.enable"
    } else {
        "credential.disable"
    };
    audited(&state, &actor, action, &access_key, result).await
}

pub async fn delete_credential(
    State(state): State<AdminState>,
    Extension(actor): Extension<Actor>,
    Path(access_key): Path<String>,
) -> Result<StatusCode> {
    let result = async {
        refuse_if_own_key(&actor, &access_key)?;
        let credential = existing_credential(&state, &access_key).await?;

        // Ownership is what grants access to a bucket, so this would strand its data.
        let others = state
            .metadata
            .list_credentials()
            .await?
            .iter()
            .any(|c| c.owner_id == credential.owner_id && c.access_key != access_key);
        let buckets = state
            .metadata
            .list_buckets(credential.owner_id)
            .await?
            .len();
        if !others && buckets > 0 {
            return Err(AdminError::conflict(format!(
                "This is the only key for an owner with {buckets} bucket(s), which nobody could \
                 reach afterwards. Disable it instead."
            )));
        }

        state.metadata.delete_credential(&access_key).await?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    audited(&state, &actor, "credential.delete", &access_key, result).await
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AuditParams {
    limit: Option<usize>,
}

pub async fn list_audit(
    State(state): State<AdminState>,
    Query(params): Query<AuditParams>,
) -> Result<Json<Vec<AuditEntry>>> {
    let limit = params
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .min(MAX_AUDIT_LIMIT);
    let entries = state.audit.recent(limit).await.map_err(|err| {
        tracing::error!(%err, "failed to read audit log");
        AdminError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read the audit log",
        )
    })?;
    Ok(Json(entries))
}
