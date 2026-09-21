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
  cargo run --bin s3-server -- --mode cluster --bootstrap

# Node 2: join through node 1
S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster \
  S3_DATA_DIR=/tmp/loony-node2 S3_BIND_ADDR=127.0.0.1:9001 \
  S3_CLUSTER_ADDR=127.0.0.1:9101 S3_ADVERTISE_ADDR=127.0.0.1:9101 \
  cargo run --bin s3-server -- --mode cluster --join 127.0.0.1:9100
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

**What's not wired together yet:** every node runs its **own independent single-voter**
Raft group for its metadata, not one shared multi-voter group spanning the cluster. The
engine that *would* make a real multi-voter group work is built and tested — what's
missing is cluster mode actually calling `add_learner`/`change_membership` to form one,
rather than each node bootstrapping its own solo group. The practical consequence:

> **A bucket created on node 1 is invisible on node 2.** Each node's bucket/object
> metadata is private to itself. Only cluster *membership* (the node registry) is
> actually shared across the cluster right now.

So today, cluster mode is genuinely useful for testing bootstrap/join/heartbeat
mechanics and the shard-transfer/RPC layer end to end — but routing S3 traffic to
multiple nodes and expecting a consistent view of your buckets across them will not work
yet. Wiring real multi-voter metadata replication (so the above stops being true) is the
next scoped piece of work, not a background task already in progress.

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
