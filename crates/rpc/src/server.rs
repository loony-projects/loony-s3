//! The internal RPC server: exposes one node's local [`ShardStore`] (PutShard/GetShard/
//! StatShard/DeleteShard) and cluster membership operations (Health/ClusterJoin/
//! ClusterMembers) to other nodes over HTTP (architecture.md §40). Mounted on a
//! separate port from the public S3 API (`S3_CLUSTER_ADDR`, not `S3_BIND_ADDR`) — S3
//! clients never see these routes, and nothing here is reachable without the shared
//! bearer token (mTLS replaces this once cluster bootstrap exists to mint a CA, see the
//! crate-level docs).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use futures::StreamExt;
use subtle::ConstantTimeEq;

use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
use s3_core::{NodeId, NodeInfo, ShardId, ShardReceipt, ShardTarget, VolumeId};
use s3_metadata::{MetadataStore, RegisterNode, Raft};
use s3_storage::{ShardBytesIn, ShardStore};

use crate::PROTOCOL_VERSION;
use crate::error::RpcServerError;
use crate::types::{HealthInfo, JoinRequest, JoinResponse};

#[derive(Clone)]
pub struct RpcServerState {
    pub shard_store: Arc<dyn ShardStore>,
    pub metadata: Arc<dyn MetadataStore>,
    pub local_node: NodeId,
    /// Shared cluster token, checked in constant time (prompt §52/§83: "signature
    /// timing attacks" — the same discipline applies to any bearer credential).
    pub token: String,
    /// This node's Raft engine handle (Phase 8), used to dispatch incoming
    /// AppendEntries/Vote/InstallSnapshot RPCs from peers. `None` for a node that isn't
    /// running Raft at all (e.g. in tests that only exercise the shard/cluster-join
    /// routes) — the raft routes answer 503 rather than panicking in that case.
    pub raft: Option<Raft>,
}

pub fn build_router(state: RpcServerState) -> Router {
    Router::new()
        .route("/internal/v1/health", get(health))
        .route("/internal/v1/cluster/join", axum::routing::post(join))
        .route("/internal/v1/cluster/members", get(members))
        .route("/internal/v1/volumes/:volume_id/shards", put(put_shard))
        .route(
            "/internal/v1/volumes/:volume_id/shards/:shard_id",
            get(get_shard).delete(delete_shard).head(stat_shard),
        )
        .route("/internal/v1/raft/append", axum::routing::post(raft_append))
        .route("/internal/v1/raft/vote", axum::routing::post(raft_vote))
        .route("/internal/v1/raft/snapshot", axum::routing::post(raft_snapshot))
        .with_state(state)
}

fn check_token(headers: &HeaderMap, expected: &str) -> Result<(), RpcServerError> {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let ok = provided.len() == expected.len()
        && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()));
    if ok {
        Ok(())
    } else {
        Err(RpcServerError::Unauthorized)
    }
}

async fn health(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
) -> Result<Json<HealthInfo>, RpcServerError> {
    check_token(&headers, &state.token)?;
    Ok(Json(HealthInfo {
        node_id: state.local_node,
        protocol_version: PROTOCOL_VERSION,
        status: "healthy".to_string(),
    }))
}

/// Handles a peer's join request: validates the claimed cluster id (if any) against
/// this node's own (architecture.md §36 — never silently merge unrelated clusters),
/// registers the joiner in the local node registry, and hands back the current member
/// list so the joiner starts with a real view instead of an empty one.
async fn join(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
    Json(req): Json<JoinRequest>,
) -> Result<Json<JoinResponse>, RpcServerError> {
    check_token(&headers, &state.token)?;

    let cluster_id = state
        .metadata
        .get_cluster_id()
        .await?
        .ok_or(RpcServerError::NotBootstrapped)?;

    if let Some(claimed) = &req.claimed_cluster_id
        && claimed != cluster_id.as_str()
    {
        return Err(s3_metadata::MetaError::ClusterIdMismatch {
            existing: cluster_id.to_string(),
            requested: claimed.clone(),
        }
        .into());
    }

    state
        .metadata
        .register_node(RegisterNode {
            node_id: req.node_id,
            advertised_address: req.advertised_address,
            failure_domain: req.failure_domain,
        })
        .await?;

    let members = state.metadata.list_nodes().await?;
    Ok(Json(JoinResponse {
        cluster_id: cluster_id.to_string(),
        members,
    }))
}

async fn members(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
) -> Result<Json<Vec<NodeInfo>>, RpcServerError> {
    check_token(&headers, &state.token)?;
    Ok(Json(state.metadata.list_nodes().await?))
}

async fn put_shard(
    State(state): State<RpcServerState>,
    Path(volume_id): Path<VolumeId>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<ShardReceipt>, RpcServerError> {
    check_token(&headers, &state.token)?;
    let target = ShardTarget {
        node_id: state.local_node,
        volume_id,
    };
    let stream: ShardBytesIn = Box::pin(
        body.into_data_stream()
            .map(|r| r.map_err(std::io::Error::other)),
    );
    let receipt = state.shard_store.put_shard(target, stream).await?;
    Ok(Json(receipt))
}

async fn get_shard(
    State(state): State<RpcServerState>,
    Path((volume_id, shard_id)): Path<(VolumeId, ShardId)>,
    headers: HeaderMap,
) -> Result<Response, RpcServerError> {
    check_token(&headers, &state.token)?;
    let target = ShardTarget {
        node_id: state.local_node,
        volume_id,
    };
    let stream = state.shard_store.get_shard(target, shard_id).await?;
    Ok(Body::from_stream(stream).into_response())
}

async fn delete_shard(
    State(state): State<RpcServerState>,
    Path((volume_id, shard_id)): Path<(VolumeId, ShardId)>,
    headers: HeaderMap,
) -> Result<StatusCode, RpcServerError> {
    check_token(&headers, &state.token)?;
    let target = ShardTarget {
        node_id: state.local_node,
        volume_id,
    };
    state.shard_store.delete_shard(target, shard_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stat_shard(
    State(state): State<RpcServerState>,
    Path((volume_id, shard_id)): Path<(VolumeId, ShardId)>,
    headers: HeaderMap,
) -> Result<Response, RpcServerError> {
    check_token(&headers, &state.token)?;
    let target = ShardTarget {
        node_id: state.local_node,
        volume_id,
    };
    match state.shard_store.stat_shard(target, shard_id).await? {
        Some(stat) => Ok((
            StatusCode::OK,
            [(header::CONTENT_LENGTH, stat.size.to_string())],
        )
            .into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

/// The three Raft RPCs (Phase 8): unlike every other handler in this file, the response
/// is always HTTP 200 with the `Result<Resp, RaftError<..>>` `openraft` itself produced
/// serialized straight into the body -- matching the upstream `raft-kv-memstore`
/// example's convention, since these `Result`s are exactly what `RaftNetwork`'s
/// `Result<Resp, RPCError<.., RemoteError<.., Err>>>` on the client side expects to
/// deserialize. Only auth and "this node isn't running Raft at all" are real HTTP-level
/// failures here.
async fn raft_append(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
    Json(req): Json<AppendEntriesRequest<s3_metadata::TypeConfig>>,
) -> Result<Response, RpcServerError> {
    check_token(&headers, &state.token)?;
    let raft = state.raft.as_ref().ok_or(RpcServerError::NotBootstrapped)?;
    Ok(Json(raft.append_entries(req).await).into_response())
}

async fn raft_vote(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
    Json(req): Json<VoteRequest<NodeId>>,
) -> Result<Response, RpcServerError> {
    check_token(&headers, &state.token)?;
    let raft = state.raft.as_ref().ok_or(RpcServerError::NotBootstrapped)?;
    Ok(Json(raft.vote(req).await).into_response())
}

async fn raft_snapshot(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
    Json(req): Json<InstallSnapshotRequest<s3_metadata::TypeConfig>>,
) -> Result<Response, RpcServerError> {
    check_token(&headers, &state.token)?;
    let raft = state.raft.as_ref().ok_or(RpcServerError::NotBootstrapped)?;
    Ok(Json(raft.install_snapshot(req).await).into_response())
}
