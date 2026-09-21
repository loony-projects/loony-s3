//! Internal node-to-node protocol (architecture.md §40): the [`ShardStore`]
//! implementation that dispatches to a remote node instead of the local disk, plus the
//! server that exposes a node's local `ShardStore` to peers. Never exposed to S3
//! clients — this is mounted on a separate port from the public S3 API.
//!
//! **Transport**: HTTP/1.1 via axum/hyper, the same stack the public S3 API already
//! uses — no protobuf/gRPC codegen tooling, and streaming request/response bodies come
//! for free (essential for shard PUT/GET, which can be megabytes). HTTP/2 (and its
//! multiplexing benefit) is a transport-layer upgrade that arrives automatically once
//! TLS/ALPN is wired in; it changes no handler code.
//!
//! **Auth**: a shared bearer token, checked in constant time. Architecture.md §41
//! commits the *production* design to mutual TLS with a cluster-minted CA — but minting
//! that CA is a cluster-bootstrap concept (Phase 7), so this phase uses the "simpler
//! credentials" dev-mode path §41 explicitly allows, and swaps to mTLS once bootstrap
//! exists to issue certs from.
//!
//! **Versioning**: the `/internal/v1/` path prefix is the negotiation mechanism
//! (prompt §88) — a future `/internal/v2/` can be served alongside it during a rolling
//! upgrade without breaking older peers.
//!
//! **Peer addressing**: `node_id -> address` isn't tracked dynamically yet (that's
//! `s3-cluster`'s job, Phase 7); [`NodeAddressResolver`] is the seam that plugs a real
//! membership-backed resolver in later without changing anything here.
//!
//! **Cluster protocol**: `join_cluster`/`fetch_members`/`fetch_health` (§40's
//! `ClusterJoin`/`Health` operations) are free functions taking an explicit base URL,
//! not methods on [`RemoteShardStore`] — they're used *before* a node is registered
//! (and therefore resolvable), most obviously during the join handshake itself.

mod client;
mod cluster_shard_store;
mod error;
mod raft_network;
mod server;
mod types;

pub const PROTOCOL_VERSION: u32 = 1;

pub use client::{
    NodeAddressResolver, RemoteShardStore, RpcClientError, StaticResolver, fetch_health,
    fetch_members, join_cluster,
};
pub use cluster_shard_store::{CachedNodeResolver, ClusterShardStore};
pub use error::RpcServerError;
pub use raft_network::HttpRaftNetworkFactory;
pub use server::{RpcServerState, build_router};
pub use types::{HealthInfo, JoinRequest, JoinResponse};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use bytes::Bytes;
    use futures::StreamExt;
    use s3_core::{ClusterId, NodeId, ShardTarget, VolumeId};
    use s3_metadata::{MetadataStore, RedbMetadataStore};
    use s3_storage::{LocalVolumeManager, ShardBytesIn, ShardStore};
    use tokio::net::TcpListener;

    use super::*;

    struct SpawnedNode {
        node_id: NodeId,
        volume_id: VolumeId,
        addr: SocketAddr,
        metadata: Arc<RedbMetadataStore>,
    }

    impl SpawnedNode {
        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    /// Starts a real RPC server on a real (ephemeral) TCP port, backed by a fresh
    /// local volume manager and metadata store -- indistinguishable, at the protocol
    /// level, from a separate OS process. `cluster_id` bootstraps the node's metadata
    /// with that cluster identity up front when given, leaving it un-bootstrapped
    /// (as a genuinely fresh node would be) when `None`.
    async fn spawn_node(token: &str, cluster_id: Option<&str>) -> SpawnedNode {
        let node_id = NodeId::new();
        let vol_dir = tempfile::tempdir().unwrap();
        let volumes = Arc::new(
            LocalVolumeManager::open(node_id, vec![vol_dir.keep()])
                .await
                .unwrap(),
        );
        let volume_id = volumes.volume_ids().next().unwrap();

        let meta_dir = tempfile::tempdir().unwrap();
        let metadata = Arc::new(
            RedbMetadataStore::open(meta_dir.keep().join("meta.redb"))
                .await
                .unwrap(),
        );
        if let Some(id) = cluster_id {
            metadata
                .bootstrap_cluster(ClusterId::parse(id).unwrap())
                .await
                .unwrap();
        }

        let state = RpcServerState {
            shard_store: volumes,
            metadata: metadata.clone(),
            local_node: node_id,
            token: token.to_string(),
            raft: None,
        };
        let router = build_router(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        SpawnedNode {
            node_id,
            volume_id,
            addr,
            metadata,
        }
    }

    fn body(data: &'static [u8]) -> ShardBytesIn {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(data))]))
    }

    #[tokio::test]
    async fn put_get_stat_delete_roundtrip_across_two_real_servers() {
        let token = "test-cluster-token";
        // Node A is the one we write to; node B exists only to prove the resolver and
        // client genuinely distinguish between multiple peers.
        let node_a = spawn_node(token, Some("c1")).await;
        let node_b = spawn_node(token, Some("c1")).await;

        let mut addresses = HashMap::new();
        addresses.insert(node_a.node_id, node_a.base_url());
        addresses.insert(node_b.node_id, node_b.base_url());
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, token.to_string());

        let target = ShardTarget {
            node_id: node_a.node_id,
            volume_id: node_a.volume_id,
        };

        let receipt = client
            .put_shard(target, body(b"hello over the wire"))
            .await
            .unwrap();
        assert_eq!(receipt.size, 19);

        let stat = client
            .stat_shard(target, receipt.shard_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stat.size, 19);

        let mut stream = client.get_shard(target, receipt.shard_id).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"hello over the wire");

        client.delete_shard(target, receipt.shard_id).await.unwrap();
        assert_eq!(
            client.stat_shard(target, receipt.shard_id).await.unwrap(),
            None
        );
        // Idempotent.
        client.delete_shard(target, receipt.shard_id).await.unwrap();

        client.health(node_a.node_id).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let node_a = spawn_node("correct-token", Some("c1")).await;
        let mut addresses = HashMap::new();
        addresses.insert(node_a.node_id, node_a.base_url());
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, "wrong-token".to_string());

        let target = ShardTarget {
            node_id: node_a.node_id,
            volume_id: node_a.volume_id,
        };
        let err = client.put_shard(target, body(b"x")).await.unwrap_err();
        assert!(matches!(err, s3_storage::StorageError::Remote(_, _)));
    }

    #[tokio::test]
    async fn get_of_unknown_shard_is_not_found() {
        let node_a = spawn_node("t", Some("c1")).await;
        let mut addresses = HashMap::new();
        addresses.insert(node_a.node_id, node_a.base_url());
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, "t".to_string());

        let target = ShardTarget {
            node_id: node_a.node_id,
            volume_id: node_a.volume_id,
        };
        let err = client
            .get_shard(target, s3_core::ShardId::new())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, s3_storage::StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn unresolvable_node_fails_fast_without_a_network_call() {
        let resolver = Arc::new(StaticResolver::new(HashMap::new()));
        let client = RemoteShardStore::new(resolver, "t".to_string());
        let target = ShardTarget {
            node_id: NodeId::new(),
            volume_id: VolumeId::new(),
        };

        let err = client.put_shard(target, body(b"x")).await.unwrap_err();
        assert!(matches!(err, s3_storage::StorageError::Unreachable(_, _)));
    }

    #[tokio::test]
    async fn a_fresh_node_can_join_a_bootstrapped_seed() {
        let token = "cluster-token";
        let seed = spawn_node(token, Some("prod")).await;
        let http = reqwest::Client::new();

        let joiner_id = NodeId::new();
        let response = join_cluster(
            &http,
            &seed.base_url(),
            token,
            &JoinRequest {
                node_id: joiner_id,
                advertised_address: "joiner.example:9100".into(),
                failure_domain: vec!["rack:b".into()],
                claimed_cluster_id: None,
                volumes: vec![],
            },
        )
        .await
        .unwrap();

        assert_eq!(response.cluster_id, "prod");
        assert_eq!(response.members.len(), 1);
        assert_eq!(response.members[0].node_id, joiner_id);

        // The seed's registry genuinely changed, not just the response payload.
        let members = seed.metadata.list_nodes().await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].advertised_address, "joiner.example:9100");

        let via_rpc = fetch_members(&http, &seed.base_url(), token).await.unwrap();
        assert_eq!(via_rpc.len(), 1);
    }

    #[tokio::test]
    async fn joining_with_a_mismatched_cluster_id_is_rejected() {
        let token = "cluster-token";
        let seed = spawn_node(token, Some("prod")).await;
        let http = reqwest::Client::new();

        let err = join_cluster(
            &http,
            &seed.base_url(),
            token,
            &JoinRequest {
                node_id: NodeId::new(),
                advertised_address: "joiner.example:9100".into(),
                failure_domain: vec![],
                claimed_cluster_id: Some("staging".into()),
                volumes: vec![],
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, RpcClientError::Remote { status: 409, .. }));
        assert!(seed.metadata.list_nodes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn joining_an_unbootstrapped_node_fails_clearly() {
        let token = "cluster-token";
        let seed = spawn_node(token, None).await;
        let http = reqwest::Client::new();

        let err = join_cluster(
            &http,
            &seed.base_url(),
            token,
            &JoinRequest {
                node_id: NodeId::new(),
                advertised_address: "joiner.example:9100".into(),
                failure_domain: vec![],
                claimed_cluster_id: None,
                volumes: vec![],
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, RpcClientError::Remote { status: 503, .. }));
    }

    #[tokio::test]
    async fn fetch_health_reports_the_responding_nodes_identity() {
        let node_a = spawn_node("t", Some("c1")).await;
        let http = reqwest::Client::new();

        let info = fetch_health(&http, &node_a.base_url(), "t").await.unwrap();
        assert_eq!(info.node_id, node_a.node_id);
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    }
}
