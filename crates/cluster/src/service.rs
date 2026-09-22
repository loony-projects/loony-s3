//! Cluster bootstrap/join orchestration and heartbeat-driven failure detection
//! (architecture.md §17-18).
//!
//! **Phase 7 scope, stated plainly**: there is no real multi-voter Raft yet (that's
//! Phase 8), so there is exactly one metadata store whose node registry is
//! cluster-wide-authoritative — whichever node bootstrapped. A node that joins instead
//! of bootstrapping does not become a second authority; it caches the bootstrap node's
//! view and refreshes it periodically. This is not a workaround that Phase 8 will need
//! to undo: `register_node`/`update_node_state` are already `MetadataStore` commands
//! (architecture.md §5/§26), so once Phase 8 replicates them across a multi-voter
//! group, every node's local metadata *becomes* authoritative automatically, with no
//! change to this crate. Failure detection is correspondingly single-observer (the
//! authority heartbeats every peer directly) rather than the multi-observer gossip
//! architecture.md §18 describes for the post-Raft world.
//!
//! One consequence worth being explicit about: in Phase 7, each node's bucket/object
//! metadata (used by `loony-object`/`loony-api`) is *not* shared across the cluster — a node
//! that joins is visible in cluster membership, but its buckets are still only visible
//! to itself. Unifying that is Phase 8 (replication) + Phase 9 (distributed PUT/GET).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use loony_core::{ClusterId, NodeId, NodeInfo, NodeState, VolumeId};
use loony_metadata::{MetadataStore, RegisterNode};
use loony_rpc::{JoinRequest, fetch_health, fetch_members, join_cluster};
use tokio::sync::RwLock;

use crate::error::ClusterError;
use crate::membership::ClusterMembership;

enum Role {
    /// This node bootstrapped the cluster (or is the only node so far): its local
    /// `MetadataStore` is the cluster-wide-authoritative node registry.
    Authority { metadata: Arc<dyn MetadataStore> },
    /// This node joined through `authority_base_url`: `cache` is a periodically
    /// refreshed read-through copy of the authority's registry.
    Member {
        authority_base_url: String,
        cache: RwLock<Vec<NodeInfo>>,
    },
}

pub struct ClusterMembershipService {
    local_node_id: NodeId,
    role: Role,
    http: reqwest::Client,
    token: String,
}

impl ClusterMembershipService {
    /// Mints a fresh cluster identity, registers this node as its first member, and
    /// marks it healthy immediately (a node never needs to heartbeat itself).
    pub async fn bootstrap(
        cluster_id: ClusterId,
        metadata: Arc<dyn MetadataStore>,
        local_node_id: NodeId,
        advertised_address: String,
        failure_domain: Vec<String>,
        volumes: Vec<VolumeId>,
        token: String,
    ) -> Result<Self, ClusterError> {
        metadata.bootstrap_cluster(cluster_id).await?;
        metadata
            .register_node(RegisterNode {
                node_id: local_node_id,
                advertised_address,
                failure_domain,
                volumes,
            })
            .await?;
        metadata
            .update_node_state(local_node_id, NodeState::Healthy)
            .await?;

        Ok(Self {
            local_node_id,
            role: Role::Authority { metadata },
            http: reqwest::Client::new(),
            token,
        })
    }

    /// Registers this node with an existing cluster via `seed_base_url`, learning the
    /// cluster id from the response rather than assuming one.
    pub async fn join(
        seed_base_url: String,
        token: String,
        local_node_id: NodeId,
        advertised_address: String,
        failure_domain: Vec<String>,
        volumes: Vec<VolumeId>,
        claimed_cluster_id: Option<ClusterId>,
    ) -> Result<(Self, ClusterId), ClusterError> {
        let http = reqwest::Client::new();
        let response = join_cluster(
            &http,
            &seed_base_url,
            &token,
            &JoinRequest {
                node_id: local_node_id,
                advertised_address,
                failure_domain,
                claimed_cluster_id: claimed_cluster_id.map(|c| c.to_string()),
                volumes,
            },
        )
        .await?;

        let cluster_id = ClusterId::parse(response.cluster_id)?;
        let service = Self {
            local_node_id,
            role: Role::Member {
                authority_base_url: seed_base_url,
                cache: RwLock::new(response.members),
            },
            http,
            token,
        };
        Ok((service, cluster_id))
    }

    /// Re-fetches the member list from the authority. A no-op for the authority itself
    /// (its metadata is already the live view); a `Member` calls this periodically
    /// (e.g. from its own heartbeat-adjacent loop) to keep its cache fresh.
    pub async fn refresh(&self) -> Result<(), ClusterError> {
        if let Role::Member {
            authority_base_url,
            cache,
        } = &self.role
        {
            let members = fetch_members(&self.http, authority_base_url, &self.token).await?;
            *cache.write().await = members;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ClusterMembership for ClusterMembershipService {
    fn local_node_id(&self) -> NodeId {
        self.local_node_id
    }

    async fn members(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        match &self.role {
            Role::Authority { metadata } => Ok(metadata.list_nodes().await?),
            Role::Member { cache, .. } => Ok(cache.read().await.clone()),
        }
    }
}

const SUSPECT_AFTER_MISSES: u32 = 3;
const OFFLINE_AFTER_MISSES: u32 = 6;

fn peer_url(advertised_address: &str) -> String {
    format!("http://{advertised_address}")
}

/// One heartbeat round: ping every known peer (except self, and except nodes already
/// being drained/removed), and apply state transitions with simple hysteresis
/// (architecture.md §33: one missed heartbeat never flips authoritative state).
/// Exposed separately from the infinite loop below so it's directly testable.
async fn heartbeat_tick(
    metadata: &Arc<dyn MetadataStore>,
    http: &reqwest::Client,
    token: &str,
    local_node_id: NodeId,
    misses: &mut HashMap<NodeId, u32>,
) {
    let members = match metadata.list_nodes().await {
        Ok(members) => members,
        Err(err) => {
            tracing::warn!(error = %err, "heartbeat: failed to list nodes");
            return;
        }
    };

    for member in members {
        if member.node_id == local_node_id
            || matches!(member.state, NodeState::Draining | NodeState::Removed)
        {
            continue;
        }

        let healthy = fetch_health(http, &peer_url(&member.advertised_address), token)
            .await
            .is_ok();
        let count = misses.entry(member.node_id).or_insert(0);

        if healthy {
            let was_unhealthy = *count > 0 || member.state != NodeState::Healthy;
            *count = 0;
            if was_unhealthy && member.state != NodeState::Healthy {
                apply_state(metadata, member.node_id, NodeState::Healthy).await;
            }
            continue;
        }

        *count += 1;
        let target_state = if *count >= OFFLINE_AFTER_MISSES {
            Some(NodeState::Offline)
        } else if *count >= SUSPECT_AFTER_MISSES {
            Some(NodeState::Suspect)
        } else {
            None
        };
        if let Some(target_state) = target_state
            && member.state != target_state
        {
            tracing::warn!(node_id = %member.node_id, ?target_state, misses = *count, "peer health degraded");
            apply_state(metadata, member.node_id, target_state).await;
        }
    }
}

async fn apply_state(metadata: &Arc<dyn MetadataStore>, node_id: NodeId, state: NodeState) {
    if let Err(err) = metadata.update_node_state(node_id, state).await {
        tracing::warn!(%node_id, ?state, error = %err, "failed to persist node state transition");
    }
}

/// Runs heartbeats forever. A no-op for a `Member` node in Phase 7 — only the
/// authority's observations can drive an authoritative state change, since only its
/// metadata is cluster-wide-visible (see module docs).
pub async fn run_heartbeat_loop(service: Arc<ClusterMembershipService>, interval: Duration) {
    let metadata = match &service.role {
        Role::Authority { metadata } => metadata.clone(),
        Role::Member { .. } => return,
    };
    let mut misses = HashMap::new();
    loop {
        tokio::time::sleep(interval).await;
        heartbeat_tick(
            &metadata,
            &service.http,
            &service.token,
            service.local_node_id,
            &mut misses,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loony_metadata::RedbMetadataStore;

    async fn open_metadata() -> Arc<RedbMetadataStore> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(
            RedbMetadataStore::open(dir.keep().join("meta.redb"))
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn bootstrap_registers_self_as_healthy() {
        let metadata = open_metadata().await;
        let node_id = NodeId::new();
        let service = ClusterMembershipService::bootstrap(
            ClusterId::parse("c1").unwrap(),
            metadata.clone(),
            node_id,
            "self:9100".into(),
            vec![],
            vec![],
            "token".into(),
        )
        .await
        .unwrap();

        let members = service.members().await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].node_id, node_id);
        assert_eq!(members[0].state, NodeState::Healthy);
        assert_eq!(service.local_node_id(), node_id);
    }

    #[tokio::test]
    async fn heartbeat_tick_marks_an_unreachable_peer_suspect_then_offline() {
        let metadata = open_metadata().await;
        let local = NodeId::new();
        let peer = NodeId::new();

        metadata
            .bootstrap_cluster(ClusterId::parse("c1").unwrap())
            .await
            .unwrap();
        metadata
            .register_node(RegisterNode {
                node_id: local,
                advertised_address: "local:9100".into(),
                failure_domain: vec![],
                volumes: vec![],
            })
            .await
            .unwrap();
        metadata
            .update_node_state(local, NodeState::Healthy)
            .await
            .unwrap();
        metadata
            .register_node(RegisterNode {
                // 127.0.0.1:1 is never going to accept a connection in a test sandbox.
                node_id: peer,
                advertised_address: "127.0.0.1:1".into(),
                failure_domain: vec![],
                volumes: vec![],
            })
            .await
            .unwrap();
        metadata
            .update_node_state(peer, NodeState::Healthy)
            .await
            .unwrap();

        let http = reqwest::Client::new();
        let mut misses = HashMap::new();
        let metadata_dyn: Arc<dyn MetadataStore> = metadata.clone();

        for _ in 0..SUSPECT_AFTER_MISSES {
            heartbeat_tick(&metadata_dyn, &http, "token", local, &mut misses).await;
        }
        let state = metadata
            .list_nodes()
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.node_id == peer)
            .unwrap()
            .state;
        assert_eq!(state, NodeState::Suspect);

        for _ in SUSPECT_AFTER_MISSES..OFFLINE_AFTER_MISSES {
            heartbeat_tick(&metadata_dyn, &http, "token", local, &mut misses).await;
        }
        let state = metadata
            .list_nodes()
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.node_id == peer)
            .unwrap()
            .state;
        assert_eq!(state, NodeState::Offline);
    }

    #[tokio::test]
    async fn heartbeat_tick_never_pings_itself() {
        let metadata = open_metadata().await;
        let local = NodeId::new();
        metadata
            .bootstrap_cluster(ClusterId::parse("c1").unwrap())
            .await
            .unwrap();
        metadata
            .register_node(RegisterNode {
                node_id: local,
                advertised_address: "127.0.0.1:1".into(), // would fail if ever pinged
                failure_domain: vec![],
                volumes: vec![],
            })
            .await
            .unwrap();
        metadata
            .update_node_state(local, NodeState::Healthy)
            .await
            .unwrap();

        let http = reqwest::Client::new();
        let mut misses = HashMap::new();
        let metadata_dyn: Arc<dyn MetadataStore> = metadata.clone();
        heartbeat_tick(&metadata_dyn, &http, "token", local, &mut misses).await;

        // Still healthy: the unreachable address was never actually contacted because
        // it belongs to the local node.
        let state = metadata
            .list_nodes()
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.node_id == local)
            .unwrap()
            .state;
        assert_eq!(state, NodeState::Healthy);
        assert!(misses.is_empty());
    }
}
