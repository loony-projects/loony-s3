# Cluster mode

How to bring up more than one node, and — just as important — a precise statement of
what "cluster" does and doesn't mean in the current codebase. Read the second half
before building anything on top of this.

## Bringing up a cluster

Bootstrap the first node, then have others join it. Each node needs its own
`S3_DATA_DIR`, its own `S3_BIND_ADDR`/`S3_CLUSTER_ADDR`, and a shared
`S3_CLUSTER_TOKEN`.

```bash
# Node 1: bootstrap a brand-new cluster
S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster S3_CLUSTER_ID=my-cluster \
  S3_DATA_DIR=/tmp/loony-node1 S3_BIND_ADDR=127.0.0.1:9000 \
  S3_CLUSTER_ADDR=127.0.0.1:9100 S3_ADVERTISE_ADDR=127.0.0.1:9100 \
  cargo run --bin loony-server -- --mode cluster --bootstrap

# Node 2: join through node 1
S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster \
  S3_DATA_DIR=/tmp/loony-node2 S3_BIND_ADDR=127.0.0.1:9001 \
  S3_CLUSTER_ADDR=127.0.0.1:9101 S3_ADVERTISE_ADDR=127.0.0.1:9101 \
  cargo run --bin loony-server -- --mode cluster --join 127.0.0.1:9100
```

`S3_ADVERTISE_ADDR` is what gets told to *other* nodes — it has to be reachable from
them, not just from `localhost`. `--bootstrap` requires an empty `S3_DATA_DIR` and a
cluster id (`--cluster-id`/`S3_CLUSTER_ID`); `--join` learns the cluster id from the seed
node's response instead. See [configuration.md](configuration.md) for the full option
list.

Each node still serves the S3 API on its own `S3_BIND_ADDR` — there's no built-in load
balancer or single cluster-wide endpoint; point clients at whichever node(s) you want to
receive traffic (see the next section for why this matters more than it would in a fully
built-out cluster).

## What this does and doesn't mean today

Stated plainly, because "cluster mode" undersells how much is and isn't actually
distributed right now:

**What's real:**
- Bootstrap/join over the internal RPC transport, with a shared node registry — every
  node sees the same member list, with states (`Joining`/`Healthy`/`Suspect`/`Offline`)
  driven by heartbeat-based failure detection.
- Node-to-node shard transfer (`PutShard`/`GetShard`/`StatShard`/`DeleteShard`) over the
  same authenticated internal RPC transport.
- A real `openraft`-backed consensus engine (Phase 8) — genuine leader election, log
  replication, snapshotting, and recovery, tested against a real 3-node cluster over
  actual HTTP (`crates/rpc/tests/raft_cluster.rs`).
- **Bucket and object *metadata* genuinely replicates across nodes.** A `--join` adds
  the joining node as a real `openraft` learner of the bootstrap node's metadata group
  (`Raft::add_learner`, called from the `join` RPC handler) rather than each node running
  its own independent single-voter group — so a bucket created on node 1, including
  ones that existed *before* node 2 ever joined, is visible via `ListBuckets`/
  `ListObjectsV2`/`HeadObject` on node 2 too. Verified against real, separate
  `loony-server` processes, not just in-process tests
  (`crates/rpc/tests/cluster_join_replication.rs` covers the same scenario
  automatically). This closes the gap this doc used to describe as "a bucket created on
  node 1 is invisible on node 2."

  Getting here surfaced a real bug worth naming, since it's the kind of thing that only
  shows up once a second voter/learner actually exists: `CreateBucket` (and
  `BeginMultipart`/`CompleteMultipart`) used to mint their id (`BucketId::new()` etc.)
  *inside* the state machine's `apply()` — which every replica runs independently. A
  single-voter group never noticed, because there was only ever one application of
  `apply()` to disagree with. The fix resolves every id and timestamp once, on
  whichever node first proposes the command, and carries it inside the command itself;
  every replica's `apply()` is now a pure function of the command, as a replicated state
  machine requires.

- **Shard bytes now genuinely spread across nodes and are fetchable through any of
  them (Phase 9).** `ObjectService` places each stripe's shards with `loony-placement`, a
  rendezvous-hashing (HRW) engine following architecture.md §10: every candidate
  `(node, volume)` pair across all `Healthy` cluster members is scored from a hash of
  the stripe's identity and the candidate, and the top-scoring distinct nodes are
  chosen (falling back to co-location on the same node only when there aren't enough
  distinct ones — a deliberate simplification, see the module doc comment in
  `crates/placement/src/lib.rs`, kept so standalone mode's existing
  multiple-local-volumes erasure coding still works). A new `ClusterShardStore`
  (`crates/rpc/src/cluster_shard_store.rs`) then routes every shard PUT/GET/STAT/DELETE
  to local disk or, over the existing internal RPC transport (`RemoteShardStore`,
  Phase 6), to whichever node actually holds it — resolved via a `CachedNodeResolver`
  that refreshes from `MetadataStore::list_nodes()` every 5 seconds. Verified against
  two real, separate `loony-server` processes with the AWS CLI: a multi-megabyte PUT
  issued against node 1 leaves shard files on *both* nodes' local disks, and `GET` of
  that object issued against *either* node returns byte-identical data (checked with
  `sha256sum` against the source file in both directions).

**What's still not wired up:** a joining node becomes a **learner**, never
automatically a **voter** — matching architecture.md §5's documented design (a small,
explicit voter set; everything else is a learner at most). There's no promotion path
yet (no `change_membership` call anywhere), so a single-voter group's leader can never
fail over to a second node today — if the bootstrap node goes down, the cluster's
metadata group has no live voter left, even though a learner might have a fully
caught-up copy of the data. Multi-voter failover is real future work, not a background
task in progress. A practical consequence today: writes (`Raft::client_write`) only
succeed when issued against whichever node currently holds leadership — a non-leader
node returns an error rather than forwarding the request to the leader, so clients
doing writes need to know (or discover) which node that is. Reads and already-placed
shard fetches work against any node.

PUT still requires every planned shard write to succeed — there's no partial
write-quorum/abort behavior from architecture.md §11, and no degraded-write
healing-job enqueueing from §12-14; both remain future work.

Internal RPC authentication is a shared bearer token (`S3_CLUSTER_TOKEN`), checked in
constant time — not the mutual TLS the architecture doc commits to for a production
deployment. Fine for a trusted local network or CI, not for anything exposed beyond
that.

## Failure detection

The bootstrapping node acts as the single authority for the node registry; every other
node's view is a periodically-refreshed cache of it. The authority heartbeats every peer
directly (not gossip-based) with simple hysteresis: a node needs 3 consecutive missed
heartbeats to flip to `Suspect` and 6 to flip to `Offline`, so one blip in an otherwise
healthy network doesn't cause churn. A node that restarts and rejoins gets a bumped
`generation` number in the registry, distinguishing it from its pre-restart incarnation.
