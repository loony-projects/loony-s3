# loony-cluster

A production-oriented, S3-compatible object storage system in Rust that runs as a
single-process standalone server or as a multi-node fault-tolerant cluster from the same
codebase.

**Status: Phases 1-5 complete (standalone mode).** Config/logging/metrics/node identity
(Phase 1), a redb-backed metadata state machine (Phase 2), the real S3 API over a real
HTTP server — CreateBucket/DeleteBucket/HeadBucket/ListBuckets/PutObject/GetObject/
HeadObject/DeleteObject/ListObjectsV2 (Phase 3), SigV4 authentication (header +
presigned URLs) with ownership-based authorization (Phase 4), and real Reed-Solomon
erasure coding with small-object replication, streaming stripe encode/decode across
multiple local volumes, and checksum-verified degraded reads (Phase 5) are all
implemented, tested, and verified against the real AWS CLI (including corrupting a
shard's bytes on disk and confirming GET still returns byte-perfect data via
reconstruction). Cluster mode (Raft, internal RPC, membership, multi-node placement,
healing) is not wired yet — that's Phases 6-9+. Multipart upload and Range requests are
also not implemented yet (separate, later-scoped phases). Read
[`docs/architecture.md`](docs/architecture.md) before writing or reviewing any code in
`crates/` — it is the design baseline every phase must stay consistent with.

```bash
S3_MODE=standalone S3_DATA_DIR=/tmp/loony-dev cargo run --bin s3-server
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
- `rpc` — internal mTLS node-to-node protocol
- `cluster` — membership, heartbeats, bootstrap/join
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
