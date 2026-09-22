# loony-cluster

A production-oriented, S3-compatible object storage system in Rust that runs as a
single-process standalone server or as a multi-node fault-tolerant cluster from the same
codebase.

**Status: Phases 1-10 complete — multipart upload is now real and cluster-safe.**
Config/logging/metrics/node identity (Phase 1), a redb-backed metadata state machine
(Phase 2), the real S3 API over a real HTTP server —
CreateBucket/DeleteBucket/HeadBucket/ListBuckets/PutObject/GetObject/HeadObject/
DeleteObject/ListObjectsV2 (Phase 3), SigV4 authentication (header + presigned URLs)
with ownership-based authorization (Phase 4), real Reed-Solomon erasure coding with
small-object replication, streaming stripe encode/decode across multiple local
volumes, and checksum-verified degraded reads (Phase 5), a node-to-node internal RPC
transport — PutShard/GetShard/StatShard/DeleteShard/Health over HTTP (Phase 6),
cluster bootstrap/join with heartbeat-driven failure detection (Phase 7), and a real
`openraft`-backed metadata engine — `RaftMetadataStore` wraps the same state machine
Phase 2 ran directly, so every mutation (standalone included: a single-voter group is
still a real Raft group now, not a WAL-equivalence argument) is proposed through
`Raft::client_write` and only applied once committed (Phase 8) — are all implemented
and tested. `loony-server --mode cluster` now really bootstraps or joins, registers in a
shared node registry, and runs a background heartbeat loop. Verified across three
genuinely separate OS processes: bootstrap → two joins → heartbeats confirming all
three `Healthy` → killing one node's process → it's detected `Offline` within one
detection window while the other two stay `Healthy` → restarting it → clean rejoin
(generation bumped) → re-confirmed `Healthy`. Phase 3-5 work is additionally verified
against the real AWS CLI (including corrupting a shard's bytes on disk and confirming
GET still returns byte-perfect data via reconstruction); Phase 8's engine is verified
by a real 3-node cluster test (`crates/rpc/tests/raft_cluster.rs`) over the actual HTTP
transport — leader election, replication to every voter, leader failure → new
election → writes keep committing, a lone partitioned node's writes timing out rather
than hanging forever, and a node that fell behind past a log purge catching up via
`InstallSnapshot` — plus a real standalone-server smoke test (AWS CLI PUT/GET/DELETE,
then a process restart proving the bucket survives via the persisted Raft log).

**Since Phase 8: cluster metadata replication is wired up.** A `--join` now adds the
joining node as a real `openraft` learner of the bootstrap node's metadata group
(`Raft::add_learner`, called from the internal `join` RPC handler) instead of each node
running its own independent single-voter group — so bucket/object metadata genuinely
replicates: a bucket (and its objects) created on node 1 before node 2 ever joined is
visible via `ListBuckets`/`ListObjectsV2`/`HeadObject` on node 2 too, verified against
real, separate OS processes and covered by an automated test
(`crates/rpc/tests/cluster_join_replication.rs`). That work also surfaced and fixed a
real bug: `CreateBucket`/`BeginMultipart`/`CompleteMultipart` used to mint their id
*inside* `apply()`, which every replica runs independently — invisible with one voter,
a real divergence the moment a second one existed. IDs and timestamps are now resolved
once, by whichever node proposes the command, and carried inside it.

**Since Phase 8: distributed PUT/GET (Phase 9) is wired up.** `loony-object` now places
each stripe's shards with a real rendezvous-hashing engine (`loony-placement`, §10)
across every `Healthy` node's volumes, not just this node's own — preferring distinct
nodes and falling back to co-location only when there aren't enough. A new
`ClusterShardStore` (`crates/rpc`) dispatches each shard read/write to local disk or,
via `RemoteShardStore` over the existing internal RPC transport, to whichever node
actually holds it, resolved through a `CachedNodeResolver` that's refreshed from the
(Raft-replicated) node registry every 5s. Verified against two genuinely separate
`loony-server` processes with the real AWS CLI: a multi-megabyte PUT issued against node 1
lands shards on both nodes' local disks, and a GET of that object issued against
*either* node returns byte-identical data (`sha256sum` compared against the source
file both ways).

**Since Phase 9: multipart upload (Phase 10) is wired up.** CreateMultipartUpload/
UploadPart/ListParts/CompleteMultipartUpload/AbortMultipartUpload are all implemented
as query-param variants of the existing `/{bucket}/{key}` route (matching real S3's own
shape). Multipart state (`(bucket, key, content_type, user_metadata)` plus every
recorded part) lives in the same Raft-replicated metadata store as everything else, so
any node can continue an upload another node started — no coordinator affinity. Each
part streams through the exact same durable, placed encode-and-write pipeline as a
whole-object PUT (small-object replication or streaming erasure coding, unchanged from
Phase 5/9); `CompleteMultipartUpload` validates the requested part list against
recorded `RecordPart` history (rejecting out-of-order or mismatched-ETag parts with the
same `InvalidPart`/`InvalidPartOrder` codes real S3 uses) and commits the concatenated
result with a single `CommitManifest` — the same atomic-visibility boundary a normal
PUT already uses, not a separate protocol. Verified against a real `loony-server` process
with the AWS CLI's low-level `s3api` commands (the high-level `s3 cp` transfer manager's
*download* side uses concurrent HTTP Range requests above the same size threshold, which
this server doesn't support yet — a pre-existing, already-documented gap, not something
multipart introduced): a 3-part, 12 MiB upload round-trips byte-identical, and the
`InvalidPart`/`InvalidPartOrder`/`NoSuchUpload` error paths all return the correct S3
error codes.

**What's still not there**: a learner is never automatically promoted to a voter (no
`change_membership` call exists yet), so a single-voter group's leader still can't fail
over to a second node — matching architecture.md §5's documented design (a small,
explicit voter set) but meaning today's cluster mode has no real HA story yet, and, in
practice, writes (which go through `Raft::client_write`) only succeed against whichever
node is currently the metadata leader — a non-leader node returns an error rather than
forwarding the request. PUT (and each multipart part) still requires every planned
shard write to succeed; there's no partial-write-quorum/abort semantics from
architecture.md §11 and no degraded-write healing-job enqueueing from §12-14 yet.
Internal RPC auth is still a bearer token, not the mTLS the architecture doc commits to
for production. HTTP Range requests, object versioning, and `x-amz-meta-*` response
headers on GET/HEAD (user metadata is stored and preserved correctly through PUT and
multipart, just never echoed back yet — a pre-existing gap, not new to Phase 10) all
remain unimplemented.

**Docs:** [`docs/usage.md`](docs/usage.md) for how to build, run, and talk to it (AWS
CLI, boto3, the web UI); [`docs/configuration.md`](docs/configuration.md) for every env
var/flag; [`docs/api-reference.md`](docs/api-reference.md) for the exact S3 API surface;
[`docs/cluster.md`](docs/cluster.md) for running more than one node. Read
[`docs/architecture.md`](docs/architecture.md) before writing or reviewing any code in
`crates/` — it is the design baseline every phase must stay consistent with.

```bash
# Standalone
S3_MODE=standalone S3_DATA_DIR=/tmp/loony-dev cargo run --bin loony-server

# Cluster: bootstrap the first node, then have others join it
S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster S3_CLUSTER_ID=my-cluster \
  S3_DATA_DIR=/tmp/loony-node1 S3_BIND_ADDR=127.0.0.1:9000 \
  S3_CLUSTER_ADDR=127.0.0.1:9100 S3_ADVERTISE_ADDR=127.0.0.1:9100 \
  cargo run --bin loony-server -- --mode cluster --bootstrap

S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster \
  S3_DATA_DIR=/tmp/loony-node2 S3_BIND_ADDR=127.0.0.1:9001 \
  S3_CLUSTER_ADDR=127.0.0.1:9101 S3_ADVERTISE_ADDR=127.0.0.1:9101 \
  cargo run --bin loony-server -- --mode cluster --join 127.0.0.1:9100
```

Set `S3_VOLUME_PATHS` (comma-separated) to configure multiple local volumes — with 6 or
more, PUT automatically uses the documented 4-data+2-parity erasure scheme; with fewer,
it adapts to a smaller scheme (see `choose_erasure_scheme` in `crates/object`) rather
than erroring. Objects under 512 KiB are replicated instead of erasure-coded
(architecture.md §9).

## Workspace

See `docs/architecture.md` §3 for the full crate-dependency diagram. Short version:

- `core` — shared domain types, no I/O
- `metadata` — Raft-backed (openraft + redb) metadata state machine
- `placement` — rendezvous-hashing shard placement
- `erasure` — Reed-Solomon streaming stripe codec (`reed-solomon-simd`)
- `storage` — local + remote (RPC) shard I/O
- `rpc` — internal node-to-node protocol (HTTP + bearer token today, mTLS still pending)
- `cluster` — cluster identity, node registry, bootstrap/join, heartbeat-driven failure detection
- `object` — Bucket/Object/Multipart/Versioning domain services
- `auth` — SigV4 + presigned URLs
- `api` — thin Axum HTTP layer
- `healing` — scrubbing, healing, GC, rebalancing
- `admin` / `admin-cli` — admin API + CLI
- `observability` — tracing/metrics
- `server` — the `server` binary, wires everything together

## Development

```bash
cargo build --workspace
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```
