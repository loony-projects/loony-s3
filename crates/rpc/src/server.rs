//! The internal RPC server: exposes one node's local [`ShardStore`] to other nodes over
//! HTTP (architecture.md §40). Mounted on a separate port from the public S3 API
//! (`S3_CLUSTER_ADDR`, not `S3_BIND_ADDR`) — S3 clients never see these routes, and
//! nothing here is reachable without the shared bearer token (mTLS replaces this once
//! cluster bootstrap exists to mint a CA, see the crate-level docs).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Serialize;
use subtle::ConstantTimeEq;

use s3_core::{NodeId, ShardId, ShardReceipt, ShardTarget, VolumeId};
use s3_storage::{ShardBytesIn, ShardStore};

use crate::PROTOCOL_VERSION;
use crate::error::RpcServerError;

#[derive(Clone)]
pub struct RpcServerState {
    pub shard_store: Arc<dyn ShardStore>,
    pub local_node: NodeId,
    /// Shared cluster token, checked in constant time (prompt §52/§83: "signature
    /// timing attacks" — the same discipline applies to any bearer credential).
    pub token: String,
}

pub fn build_router(state: RpcServerState) -> Router {
    Router::new()
        .route("/internal/v1/health", get(health))
        .route("/internal/v1/volumes/:volume_id/shards", put(put_shard))
        .route(
            "/internal/v1/volumes/:volume_id/shards/:shard_id",
            get(get_shard).delete(delete_shard).head(stat_shard),
        )
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

#[derive(Serialize)]
struct HealthResponse {
    node_id: NodeId,
    protocol_version: u32,
    status: &'static str,
}

async fn health(
    State(state): State<RpcServerState>,
    headers: HeaderMap,
) -> Result<Json<HealthResponse>, RpcServerError> {
    check_token(&headers, &state.token)?;
    Ok(Json(HealthResponse {
        node_id: state.local_node,
        protocol_version: PROTOCOL_VERSION,
        status: "healthy",
    }))
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
