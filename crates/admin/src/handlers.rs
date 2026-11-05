use std::collections::{BTreeMap, HashSet};

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use loony_core::{BucketName, NodeId, NodeState, OwnerId};
use loony_metadata::{Credential, ListObjectsQuery, MetadataStore};
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
    let credentials = state.metadata.list_credentials().await?;
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
        *nodes_by_state.entry(format!("{:?}", node.state)).or_default() += 1;
    }

    Ok(Json(Overview {
        mode: state.mode,
        node_id: state.node_id,
        cluster_id: state.metadata.get_cluster_id().await?.map(|id| id.to_string()),
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
                shards: stats.shards_by_node.get(&node.node_id).copied().unwrap_or(0),
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
        state.metadata.update_node_state(node_id, NodeState::Draining).await?;
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
        state.metadata.update_node_state(node_id, NodeState::Joining).await?;
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

        state.metadata.update_node_state(node_id, NodeState::Removed).await?;
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
                let usage = stats.buckets.get(&bucket.bucket_id).copied().unwrap_or_default();
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
            created_at: cred.created_at,
        }
    }
}

pub async fn list_credentials(State(state): State<AdminState>) -> Result<Json<Vec<CredentialView>>> {
    let mut credentials = state.metadata.list_credentials().await?;
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
    (0..ids).map(|_| uuid::Uuid::new_v4().simple().to_string()).collect()
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
                let has_key = state.metadata.list_credentials().await?.iter().any(|c| c.owner_id == owner_id);
                let has_bucket = !state.metadata.list_buckets(owner_id).await?.is_empty();
                if !has_key && !has_bucket {
                    return Err(AdminError::not_found(format!("Owner {owner_id} does not exist")));
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
    let action = if request.enabled { "credential.enable" } else { "credential.disable" };
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
        let buckets = state.metadata.list_buckets(credential.owner_id).await?.len();
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
    let limit = params.limit.unwrap_or(DEFAULT_AUDIT_LIMIT).min(MAX_AUDIT_LIMIT);
    let entries = state.audit.recent(limit).await.map_err(|err| {
        tracing::error!(%err, "failed to read audit log");
        AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "Could not read the audit log")
    })?;
    Ok(Json(entries))
}
