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

mod client;
mod error;
mod server;

pub const PROTOCOL_VERSION: u32 = 1;

pub use client::{NodeAddressResolver, RemoteShardStore, StaticResolver};
pub use error::RpcServerError;
pub use server::{RpcServerState, build_router};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use bytes::Bytes;
    use futures::StreamExt;
    use s3_core::{NodeId, ShardTarget, VolumeId};
    use s3_storage::{LocalVolumeManager, ShardBytesIn, ShardStore};
    use tokio::net::TcpListener;

    use super::*;

    /// Starts a real RPC server on a real (ephemeral) TCP port, backed by a fresh
    /// local volume manager -- indistinguishable, at the protocol level, from a
    /// separate OS process.
    async fn spawn_node(token: &str) -> (NodeId, VolumeId, SocketAddr) {
        let node_id = NodeId::new();
        let dir = tempfile::tempdir().unwrap();
        let volumes = Arc::new(
            LocalVolumeManager::open(node_id, vec![dir.keep()])
                .await
                .unwrap(),
        );
        let volume_id = volumes.volume_ids().next().unwrap();

        let state = RpcServerState {
            shard_store: volumes,
            local_node: node_id,
            token: token.to_string(),
        };
        let router = build_router(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        (node_id, volume_id, addr)
    }

    fn body(data: &'static [u8]) -> ShardBytesIn {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(data))]))
    }

    #[tokio::test]
    async fn put_get_stat_delete_roundtrip_across_two_real_servers() {
        let token = "test-cluster-token";
        // "Node A" is the one we write to; "node B" exists only to prove the resolver
        // and client genuinely distinguish between multiple peers.
        let (node_a, volume_a, addr_a) = spawn_node(token).await;
        let (_node_b, _volume_b, addr_b) = spawn_node(token).await;

        let mut addresses = HashMap::new();
        addresses.insert(node_a, format!("http://{addr_a}"));
        addresses.insert(_node_b, format!("http://{addr_b}"));
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, token.to_string());

        let target = ShardTarget {
            node_id: node_a,
            volume_id: volume_a,
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

        client.health(node_a).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let (node_a, volume_a, addr_a) = spawn_node("correct-token").await;
        let mut addresses = HashMap::new();
        addresses.insert(node_a, format!("http://{addr_a}"));
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, "wrong-token".to_string());

        let target = ShardTarget {
            node_id: node_a,
            volume_id: volume_a,
        };
        let err = client.put_shard(target, body(b"x")).await.unwrap_err();
        assert!(matches!(err, s3_storage::StorageError::Remote(_, _)));
    }

    #[tokio::test]
    async fn get_of_unknown_shard_is_not_found() {
        let (node_a, volume_a, addr_a) = spawn_node("t").await;
        let mut addresses = HashMap::new();
        addresses.insert(node_a, format!("http://{addr_a}"));
        let resolver = Arc::new(StaticResolver::new(addresses));
        let client = RemoteShardStore::new(resolver, "t".to_string());

        let target = ShardTarget {
            node_id: node_a,
            volume_id: volume_a,
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
}
