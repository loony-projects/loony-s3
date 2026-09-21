//! Phase 8's own test list (PROMPT.md): "leader failure, restart, minority partition,
//! snapshot recovery" — exercised here against a real 3-node cluster, each node a real
//! `RaftMetadataStore` behind a real `axum` server on a real (loopback) TCP port,
//! talking over the actual bearer-token-authenticated HTTP transport
//! (`HttpRaftNetworkFactory`/the `raft_append`/`raft_vote`/`raft_snapshot` routes) —
//! not an in-process shortcut. This is the multi-voter case `s3-metadata`'s own
//! single-node tests can't exercise, since they deliberately don't depend on this
//! crate's network layer.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use openraft::BasicNode;
use s3_core::{BucketName, NodeId, OwnerId};
use s3_metadata::{CreateBucket, MetaError, MetadataStore, RaftMetadataStore};
use s3_rpc::{HttpRaftNetworkFactory, RpcServerState};
use s3_storage::LocalVolumeManager;
use tokio::net::TcpListener;

const TOKEN: &str = "raft-cluster-test-token";

struct Node {
    node_id: NodeId,
    addr: SocketAddr,
    store: Arc<RaftMetadataStore>,
    server: tokio::task::JoinHandle<()>,
}

impl Node {
    fn advertised(&self) -> String {
        self.addr.to_string()
    }
}

/// Binds an ephemeral port, then immediately drops the listener so the port number can
/// be reused by a later `serve()` call on the same address without holding the socket
/// open in the meantime.
async fn reserve_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

async fn serve(state: RpcServerState, addr: SocketAddr) -> tokio::task::JoinHandle<()> {
    let router = s3_rpc::build_router(state);
    let listener = TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    })
}

/// Starts one real node: a `RaftMetadataStore` over a fresh redb file, served on a real
/// TCP port through the same `axum` router (with the same bearer-token auth) production
/// nodes use. Returns before any membership is configured -- callers `initialize()` the
/// group explicitly, exactly like a real cluster bootstrap would.
async fn spawn_node() -> Node {
    let node_id = NodeId::new();
    let meta_dir = tempfile::tempdir().unwrap();
    let db_path = meta_dir.keep().join("meta.redb");
    let store = Arc::new(
        RaftMetadataStore::open(node_id, &db_path, HttpRaftNetworkFactory::new(TOKEN.to_string()))
            .await
            .unwrap(),
    );

    let vol_dir = tempfile::tempdir().unwrap();
    let volumes = Arc::new(
        LocalVolumeManager::open(node_id, vec![vol_dir.keep()])
            .await
            .unwrap(),
    );

    let addr = reserve_addr().await;
    let state = RpcServerState {
        shard_store: volumes,
        metadata: store.clone() as Arc<dyn MetadataStore>,
        local_node: node_id,
        token: TOKEN.to_string(),
        raft: Some(store.raft().clone()),
    };
    let server = serve(state, addr).await;

    Node {
        node_id,
        addr,
        store,
        server,
    }
}

async fn wait_for_leader(nodes: &[&Node], timeout: Duration) -> Option<NodeId> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        for node in nodes {
            if node.store.raft().metrics().borrow().state.is_leader() {
                return Some(node.node_id);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn leader(nodes: &[Node], leader_id: NodeId) -> &Node {
    nodes.iter().find(|n| n.node_id == leader_id).unwrap()
}

async fn create_bucket(store: &RaftMetadataStore, name: &str) -> Result<(), MetaError> {
    store
        .create_bucket(CreateBucket {
            name: BucketName::parse(name).unwrap(),
            owner_id: OwnerId::new(),
            region: "us-east-1".into(),
        })
        .await
        .map(|_| ())
}

async fn bucket_exists(store: &RaftMetadataStore, name: &str) -> bool {
    store
        .get_bucket(&BucketName::parse(name).unwrap())
        .await
        .unwrap()
        .is_some()
}

/// Brings up 3 nodes and forms them into one 3-voter group via `initialize()`, the same
/// call a real bootstrap makes for a multi-node start (architecture.md §17's "3/5
/// voters designated at bootstrap"). Returns once a leader has been elected.
async fn bring_up_three_node_cluster() -> Vec<Node> {
    let nodes = vec![spawn_node().await, spawn_node().await, spawn_node().await];

    let mut members = BTreeMap::new();
    for n in &nodes {
        members.insert(n.node_id, BasicNode::new(n.advertised()));
    }
    nodes[0].store.raft().initialize(members).await.unwrap();

    let refs: Vec<&Node> = nodes.iter().collect();
    wait_for_leader(&refs, Duration::from_secs(5))
        .await
        .expect("a 3-voter group should elect a leader");

    nodes
}

#[tokio::test]
async fn leader_is_elected_and_writes_replicate_to_every_voter() {
    let nodes = bring_up_three_node_cluster().await;
    let refs: Vec<&Node> = nodes.iter().collect();
    let leader_id = wait_for_leader(&refs, Duration::from_secs(1)).await.unwrap();
    let leader_node = leader(&nodes, leader_id);

    create_bucket(&leader_node.store, "replicated-bucket").await.unwrap();

    // Every voter's *own* redb state should reflect the write -- not just the leader's
    // -- since reads bypass Raft and hit local state directly (`store.rs`'s doc
    // comment), this only passes once the entry has genuinely replicated everywhere.
    for node in &nodes {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !bucket_exists(&node.store, "replicated-bucket").await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "bucket never replicated to node {}",
                node.node_id
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test]
async fn leader_failure_triggers_a_new_election_and_the_cluster_keeps_accepting_writes() {
    let nodes = bring_up_three_node_cluster().await;
    let refs: Vec<&Node> = nodes.iter().collect();
    let first_leader_id = wait_for_leader(&refs, Duration::from_secs(1)).await.unwrap();

    // Kill the leader's Raft engine outright -- no graceful step-down, simulating a
    // real crash rather than a clean handover.
    leader(&nodes, first_leader_id).store.raft().shutdown().await.unwrap();

    let survivors: Vec<&Node> = nodes.iter().filter(|n| n.node_id != first_leader_id).collect();
    let new_leader_id = wait_for_leader(&survivors, Duration::from_secs(10))
        .await
        .expect("the surviving majority should elect a new leader");
    assert_ne!(new_leader_id, first_leader_id);

    let new_leader = survivors.into_iter().find(|n| n.node_id == new_leader_id).unwrap();
    create_bucket(&new_leader.store, "after-failover").await.unwrap();
    assert!(bucket_exists(&new_leader.store, "after-failover").await);
}

#[tokio::test]
async fn a_lone_partitioned_node_cannot_commit_writes() {
    let nodes = bring_up_three_node_cluster().await;

    // Simulate a 1-of-3 partition by stopping the *other two* nodes' Raft engines,
    // isolating whichever single node is left with no reachable peer.
    let mut iter = nodes.iter();
    let isolated = iter.next().unwrap();
    for n in iter {
        n.store.raft().shutdown().await.unwrap();
    }

    let write_result = create_bucket(&isolated.store, "should-never-commit").await;
    assert!(
        matches!(write_result, Err(MetaError::RaftUnavailable(_))),
        "a lone node with no reachable quorum must not be able to commit a write, got {write_result:?}"
    );
}

#[tokio::test]
async fn a_partitioned_node_that_reconnects_after_falling_behind_catches_up_via_snapshot() {
    let mut nodes = bring_up_three_node_cluster().await;
    let refs: Vec<&Node> = nodes.iter().collect();
    let leader_id = wait_for_leader(&refs, Duration::from_secs(1)).await.unwrap();
    let lagging_id = nodes.iter().map(|n| n.node_id).find(|id| *id != leader_id).unwrap();
    let lagging_idx = nodes.iter().position(|n| n.node_id == lagging_id).unwrap();

    // Cut the follower's network reachability -- stop only its HTTP server, not its
    // Raft engine or redb store, mirroring what actually differs between "unreachable"
    // and "crashed" from the rest of the cluster's point of view: the leader can no
    // longer replicate to it, but nothing about the node itself is destroyed.
    let lagging = nodes.remove(lagging_idx);
    lagging.server.abort();
    let _ = lagging.server.await;
    let lagging_store = lagging.store;
    let lagging_addr = lagging.addr;

    let leader_node = leader(&nodes, leader_id);
    for i in 0..10 {
        create_bucket(&leader_node.store, &format!("pre-restart-{i}")).await.unwrap();
    }

    // Force a snapshot, then purge every log entry it covers -- so when the follower
    // comes back, the leader has nothing left to replay for it and *must* use
    // `InstallSnapshot` instead of catching it up entry-by-entry.
    leader_node.store.raft().trigger().snapshot().await.unwrap();
    let snapshot_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let metrics = leader_node.store.raft().metrics().borrow().clone();
        if let Some(snapshot) = metrics.snapshot {
            leader_node.store.raft().trigger().purge_log(snapshot.index).await.unwrap();
            break;
        }
        assert!(tokio::time::Instant::now() < snapshot_deadline, "leader never built a snapshot");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let purge_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while leader_node.store.raft().metrics().borrow().purged.is_none() {
        assert!(tokio::time::Instant::now() < purge_deadline, "leader never purged its log");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Reconnect the follower on the same address, same store: the rest of the cluster
    // still has that address in its membership config, so replication can reach it
    // again immediately.
    let vol_dir = tempfile::tempdir().unwrap();
    let volumes = Arc::new(
        LocalVolumeManager::open(lagging_id, vec![vol_dir.keep()])
            .await
            .unwrap(),
    );
    let state = RpcServerState {
        shard_store: volumes,
        metadata: lagging_store.clone() as Arc<dyn MetadataStore>,
        local_node: lagging_id,
        token: TOKEN.to_string(),
        raft: Some(lagging_store.raft().clone()),
    };
    serve(state, lagging_addr).await;

    let catchup_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let names: Vec<String> = (0..10).map(|i| format!("pre-restart-{i}")).collect();
        let all_present =
            futures::future::join_all(names.iter().map(|name| bucket_exists(&lagging_store, name)))
                .await
                .into_iter()
                .all(|present| present);
        if all_present {
            break;
        }
        assert!(
            tokio::time::Instant::now() < catchup_deadline,
            "restarted node never caught up via snapshot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
