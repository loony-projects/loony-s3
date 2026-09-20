# loony-cluster

A production-oriented, S3-compatible object storage system in Rust that runs as a
single-process standalone server or as a multi-node fault-tolerant cluster from the same
codebase.

**Status: Phases 1-7 complete — real cluster mode now runs.**
Config/logging/metrics/node identity (Phase 1), a redb-backed metadata state machine
(Phase 2), the real S3 API over a real HTTP server —
CreateBucket/DeleteBucket/HeadBucket/ListBuckets/PutObject/GetObject/HeadObject/
DeleteObject/ListObjectsV2 (Phase 3), SigV4 authentication (header + presigned URLs)
with ownership-based authorization (Phase 4), real Reed-Solomon erasure coding with
small-object replication, streaming stripe encode/decode across multiple local
volumes, and checksum-verified degraded reads (Phase 5), a node-to-node internal RPC
transport — PutShard/GetShard/StatShard/DeleteShard/Health over HTTP (Phase 6), and
cluster bootstrap/join with heartbeat-driven failure detection (Phase 7) are all
implemented and tested. `s3-server --mode cluster` now really bootstraps or joins,
registers in a shared node registry, and runs a background heartbeat loop. Verified
across three genuinely separate OS processes: bootstrap → two joins → heartbeats
confirming all three `Healthy` → killing one node's process → it's detected `Offline`
within one detection window while the other two stay `Healthy` → restarting it →
clean rejoin (generation bumped) → re-confirmed `Healthy`. Phase 3-5 work is
additionally verified against the real AWS CLI (including corrupting a shard's bytes
on disk and confirming GET still returns byte-perfect data via reconstruction).

**What "cluster" does not yet mean**, stated plainly: there is no real multi-voter Raft
(Phase 8), so cluster *membership* is shared (one authority — whichever node
bootstrapped — tracks the registry; joiners cache its view) but bucket/object metadata
is **not** yet shared — each node's buckets are only visible to itself until Phase 8
(replication) + Phase 9 (distributed PUT/GET). Internal RPC auth is still a bearer
token, not the mTLS the architecture doc commits to for production (CA-minting needs a
real bootstrap flow, which now exists, so this is the next thing to close). Multipart
upload and Range requests also remain unimplemented (separate, later-scoped phases).
Read [`docs/architecture.md`](docs/architecture.md) before writing or reviewing any
code in `crates/` — it is the design baseline every phase must stay consistent with.

```bash
# Standalone
S3_MODE=standalone S3_DATA_DIR=/tmp/loony-dev cargo run --bin s3-server

# Cluster: bootstrap the first node, then have others join it
S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster S3_CLUSTER_ID=my-cluster \
  S3_DATA_DIR=/tmp/loony-node1 S3_BIND_ADDR=127.0.0.1:9000 \
  S3_CLUSTER_ADDR=127.0.0.1:9100 S3_ADVERTISE_ADDR=127.0.0.1:9100 \
  cargo run --bin s3-server -- --mode cluster --bootstrap

S3_CLUSTER_TOKEN=shared-secret S3_MODE=cluster \
  S3_DATA_DIR=/tmp/loony-node2 S3_BIND_ADDR=127.0.0.1:9001 \
  S3_CLUSTER_ADDR=127.0.0.1:9101 S3_ADVERTISE_ADDR=127.0.0.1:9101 \
  cargo run --bin s3-server -- --mode cluster --join 127.0.0.1:9100
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
