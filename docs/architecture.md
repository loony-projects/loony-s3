# loony-cluster — Phase 0 Architecture

Status: design baseline. Nothing in `crates/` beyond empty skeletons exists yet. No
implementation proceeds until this document is internally consistent (per project rule:
architecture before Axum routes).

This is a **separate project** from `loony-rs` (the existing single-binary JWT-auth
server). It does not reuse that code; it reuses nothing but the lessons learned from it.
`loony-rs` keeps working independently.

---

## 1. System architecture

```
                         LS3 Clients (SigV4)
                                 │
                        ┌────────────────┐
                        │   api        │  thin HTTP layer, no business logic
                        └────────┬────────┘
                                 │
                        ┌────────────────┐
                        │  auth        │  SigV4 verify / presign
                        └────────┬────────┘
                                 │
                        ┌────────────────┐
                        │  object      │  Bucket/Object/Multipart/Versioning services
                        └───┬────────┬────┘
                            │        │
                 ┌──────────┘        └──────────┐
                 ▼                               ▼
          ┌─────────────┐                 ┌──────────────┐
          │ metadata │                 │  Durability   │
          │ (Raft SM)   │                 │  Layer        │
          └─────────────┘                 └──────┬────────┘
                                                    │
                                    ┌───────────────┴───────────────┐
                                    ▼                                ▼
                             placement                      erasure
                             (choose N+M targets)          (stripe encode/decode)
                                    │                                │
                                    └───────────────┬────────────────┘
                                                     ▼
                                              storage (ShardStore)
                                          local impl  │  remote impl (via rpc)
                                                     │
                                   ┌─────────────────┼─────────────────┐
                                   ▼                 ▼                 ▼
                                Node A            Node B            Node C
                               (volumes)         (volumes)         (volumes)
```

`cluster` (membership/failure-detection) and `healing` (scrub/heal/GC/rebalance)
sit beside this pipeline and act on the same `MetadataStore` + `ShardStore` traits.
`observability` and `admin` are cross-cutting.

**The single most important structural decision:** standalone is not a different code
path from cluster — it is a cluster of size one. See §2.

---

## 2. Standalone vs cluster: one architecture, not two

Both `MetadataStore` and `ShardStore` are defined as traits with exactly one production
implementation family each:

- `MetadataStore` → always backed by an `openraft` replicated state machine persisted in
  `redb`. Standalone mode runs a **Raft group of one voter** (itself). Cluster mode runs
  a Raft group of 3 or 5 designated voters. The state machine code, the command log, the
  snapshot format, and the `MetadataStore` trait impl are byte-for-byte the same code in
  both modes — only the membership config differs.
- `ShardStore` → always addressed as `(node_id, volume_id)`. Standalone mode has one
  node with N local volumes. Cluster mode has many nodes. Placement, erasure coding, and
  the write/read protocols never know or care how many nodes exist; they only see a list
  of `(node_id, volume_id)` targets returned by `PlacementEngine`. Writing to a target
  whose `node_id == self` short-circuits to a direct local-disk call inside `storage`;
  writing to a remote `node_id` goes over `rpc`. This is an implementation detail
  inside `storage`, invisible to everything above it.

This avoids the two most common failure modes of "bolted-on" distribution:
1. A second metadata implementation (e.g. plain SQLite) that has to be kept
   semantically compatible with Raft by hand.
2. LS3 handlers branching on `if cluster_mode { .. } else { .. }`.

Rejected alternative: SQLite for standalone + Raft for cluster (what §1 of the prompt
suggests as *a* valid option). Rejected because it means two metadata engines, two sets
of migration code, and a standalone→cluster migration that has to translate between
storage formats instead of just adding Raft voters. The single-voter-Raft approach costs
a small amount of standalone latency (one fsync'd log append per metadata mutation
instead of a raw SQLite transaction) in exchange for deleting an entire subsystem and a
migration problem. That trade is worth it for a system whose stated priority order is
correctness and durability before performance.

---

## 3. Cargo workspace structure

```
loony-cluster/
├── Cargo.toml                 workspace
├── crates/
│   ├── core/                domain types, newtypes, Error — no I/O, no deps on other crates
│   ├── metadata/             MetadataStore trait + openraft state machine + redb log store
│   ├── placement/            PlacementEngine trait + rendezvous-hashing impl
│   ├── erasure/               ErasureCodec trait + reed-solomon-simd streaming codec + replication codec
│   ├── storage/               ShardStore trait + local disk engine + remote (RPC) client impl
│   ├── rpc/                   internal node-to-node protocol: framing, mTLS, version negotiation
│   ├── cluster/               ClusterMembership trait, node registry, heartbeats, bootstrap/join
│   ├── object/                Bucket/Object/Multipart/Versioning domain services (the "Object Service")
│   ├── auth/                  SigV4 verification, presigned URLs, credential trait
│   ├── api/                   Axum routes, LS3 XML (de)serialization, error mapping — thin
│   ├── healing/               scrubbing, healing, GC, rebalancing background jobs
│   ├── admin/                 admin API handlers (lib, mounted by server)
│   ├── admin-cli/              `admin` binary, talks to admin API
│   ├── observability/          tracing/metrics setup, request-id propagation helpers
│   └── server/                 `server` binary: config, mode dispatch, wiring
├── migrations/                    redb schema version notes (redb has no SQL migrations;
│                                   this holds format_version upgrade code per §87)
├── tests/                         integration + cluster + failure tests
├── benches/
├── docs/
├── docker-compose.standalone.yml
├── docker-compose.cluster.yml
└── README.md
```

Dependency direction is strictly downward: `core` depends on nothing in-workspace;
`api`/`admin`/`healing` depend on `object`; `object` depends on
`metadata` + `placement` + `erasure` + `storage`; `storage` depends on
`rpc`; `cluster` depends on `rpc` + `metadata`. `server` is the only crate
allowed to depend on everything — it exists purely to wire concrete implementations
behind the traits and run the two binaries' worth of startup logic (`--mode standalone`
vs `--mode cluster` only changes *configuration*, e.g. Raft voter count and node count,
never which code runs).

---

## 4. Major traits

```rust
// metadata — the authoritative, linearizable source of truth
#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn create_bucket(&self, cmd: CreateBucket) -> Result<BucketId, MetaError>;
    async fn delete_bucket(&self, cmd: DeleteBucket) -> Result<(), MetaError>;
    async fn get_bucket(&self, name: &BucketName) -> Result<Option<Bucket>, MetaError>;
    async fn list_buckets(&self, owner: &OwnerId) -> Result<Vec<Bucket>, MetaError>;

    async fn commit_manifest(&self, cmd: CommitManifest) -> Result<VersionId, MetaError>;
    async fn get_manifest(&self, q: ManifestQuery) -> Result<Option<ObjectManifest>, MetaError>;
    async fn tombstone_object(&self, cmd: TombstoneObject) -> Result<(), MetaError>;
    async fn list_objects(&self, q: ListObjectsQuery) -> Result<ListObjectsPage, MetaError>;

    async fn begin_multipart(&self, cmd: BeginMultipart) -> Result<UploadId, MetaError>;
    async fn record_part(&self, cmd: RecordPart) -> Result<(), MetaError>;
    async fn complete_multipart(&self, cmd: CompleteMultipart) -> Result<VersionId, MetaError>;
    async fn abort_multipart(&self, cmd: AbortMultipart) -> Result<(), MetaError>;

    async fn register_node(&self, cmd: RegisterNode) -> Result<(), MetaError>;
    async fn update_node_state(&self, cmd: UpdateNodeState) -> Result<(), MetaError>;
    async fn cluster_view(&self) -> Result<ClusterView, MetaError>;

    async fn linearizable_read_barrier(&self) -> Result<(), MetaError>; // Raft read-index
}

// storage — put/get/delete a single opaque shard, local or remote
#[async_trait]
pub trait ShardStore: Send + Sync {
    async fn put_shard(&self, target: ShardTarget, data: ShardBytes) -> Result<ShardReceipt, StorageError>;
    async fn get_shard(&self, target: ShardTarget) -> Result<ShardStream, StorageError>;
    async fn stat_shard(&self, target: ShardTarget) -> Result<Option<ShardStat>, StorageError>;
    async fn delete_shard(&self, target: ShardTarget) -> Result<(), StorageError>;
}

// placement — deterministic target selection, no I/O
pub trait PlacementEngine: Send + Sync {
    fn plan_write(&self, req: PlacementRequest) -> Result<PlacementPlan, PlacementError>;
    fn targets_for_read(&self, manifest: &ObjectManifest) -> Vec<ShardTarget>;
}

// cluster — the live view of who exists and how healthy they are
#[async_trait]
pub trait ClusterMembership: Send + Sync {
    fn local_node_id(&self) -> NodeId;
    fn members(&self) -> Vec<NodeInfo>;
    fn node_state(&self, id: NodeId) -> Option<NodeState>;
    async fn report_health(&self, id: NodeId, observed: HealthSample);
}

// erasure — pure computation, no I/O
pub trait ErasureCodec: Send + Sync {
    fn scheme(&self) -> ErasureScheme; // e.g. { data: 4, parity: 2 }
    fn encode_stripe(&self, data: &[Bytes]) -> Result<Vec<Bytes>, ErasureError>;
    fn reconstruct(&self, shards: &[Option<Bytes>], scheme: ErasureScheme) -> Result<Vec<Bytes>, ErasureError>;
}
```

No trait mixes concerns: `PlacementEngine` never touches disk, `ErasureCodec` never
knows about nodes, `ShardStore` never knows about erasure math. Each is independently
unit-testable.

---

## 5. Metadata architecture & Raft

**Why openraft:** mature enough to run in production (Databend's meta-service),
actively maintained, async-native (fits Tokio), explicit snapshot + streaming API,
pluggable storage. Its API is pre-1.0 (0.10.x), so `metadata` wraps it entirely
behind `MetadataStore` — no openraft types leak past that crate boundary — so an
eventual 1.0 migration or, worst case, a swap to a different consensus crate, is
contained to one crate.

**Why redb for the Raft log + state machine:** pure Rust (no C/C++ toolchain
requirement, unlike RocksDB), stable on-disk format with an explicit upgrade path,
ACID transactions, good enough write latency for a metadata workload (metadata
mutations are small; we are not storing object bytes here). Two redb tables per node:
`raft_log` (openraft log storage) and `raft_state_machine` (applied state: buckets,
objects, manifests, multipart uploads, node registry, credentials — see §37 categories).

**Voter set is small and explicit, storage capacity is not tied to it.** A cluster
designates 3 or 5 nodes as Raft voters at bootstrap (`admin cluster bootstrap
--voters node-01,node-02,node-03`); every other node registers as cluster member with
storage volumes but is a Raft *learner* at most (receives metadata for local caching of
read-mostly data, never votes). This mirrors why systems like TiKV separate PD from
stores, and Ceph separates mons from OSDs: it keeps Raft's O(voters) coordination cost
flat while storage nodes scale to hundreds. Standalone mode is the degenerate case:
voters = {self}, learners = {}.

- **Leader election**: standard Raft leader election with pre-vote enabled (avoids
  term inflation from a partitioned-then-rejoining node).
- **Quorum**: majority of voters (2 of 3, 3 of 5).
- **Log replication**: every `MetadataStore` mutation is a Raft log entry; entries are
  small commands (`CreateBucket`, `CommitManifest`, ...), never object bytes (§105).
- **Snapshotting**: periodic snapshot of the redb state machine once the log exceeds a
  configurable entry count; snapshot transfer to lagging/rejoining voters uses openraft's
  chunked snapshot RPC over `rpc`.
- **Recovery**: a restarted node replays its redb log + snapshot; a node that was
  offline past the log-retention window receives a full snapshot instead of a replay.
- **Membership changes**: joint consensus (openraft supports this) when adding/removing
  a voter, so voter-set changes never risk a split quorum.

**Control plane vs data plane, explicitly:** the Raft log carries *decisions about*
data (a manifest is durable, a bucket exists), never the data itself. The largest
plausible Raft log entry is a multipart `CompleteMultipart` command listing shard
locations for potentially thousands of parts — still kilobytes, not gigabytes. Object
bytes only ever move through `storage`/`rpc`.

---

## 6. Object manifest design

```rust
pub struct ObjectManifest {
    pub object_id: ObjectId,
    pub bucket_id: BucketId,
    pub key: ObjectKey,
    pub version_id: VersionId,       // UUIDv7 — time-ordered, globally unique, no coordinator needed
    pub size: u64,
    pub etag: ETag,
    pub sha256: [u8; 32],
    pub content_type: String,
    pub user_metadata: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub delete_marker: bool,
    pub durability: DurabilityPolicy,  // Replicated{n} | Erasure{data,parity}
    pub parts: Vec<PartManifest>,      // len() == 1 for a normal (non-multipart) PUT
}

pub struct PartManifest {
    pub part_number: u32,
    pub offset: u64,               // logical byte offset within the object
    pub size: u64,
    pub etag: ETag,                // per-part ETag (needed for multipart ETag composition, §29)
    pub stripes: Vec<Stripe>,
}

pub struct Stripe {
    pub stripe_index: u32,
    pub stripe_offset: u64,        // byte offset within the part
    pub stripe_len: u64,           // logical (pre-encoding) length of this stripe
    pub shards: Vec<ShardLocation>,
}

pub struct ShardLocation {
    pub shard_index: u16,          // 0..data+parity
    pub node_id: NodeId,
    pub volume_id: VolumeId,
    pub shard_id: ShardId,         // opaque physical identifier, §49
    pub size: u32,
    pub checksum: [u8; 32],        // independent per-shard checksum, §30
    pub generation: u64,           // bumped on every heal/rebalance write, §32/§44
}
```

Never expose `ShardLocation`/`shard_id`/paths through the LS3 API — `api` maps
`ObjectManifest` to LS3 XML/headers and drops everything below `PartManifest`.

Multipart composition falls out for free: `CompleteMultipartUpload` builds the final
manifest by concatenating the already-durable `PartManifest`s recorded during
`UploadPart` (recomputing `offset` cumulatively) — no re-encoding, no re-upload, exactly
the "avoid downloading the complete object" requirement in §28/§27.

Range GET maps a byte range to `parts[i]` by offset+size, then to
`stripes[j]` by the same arithmetic within the part, and only fetches shards for the
intersecting stripes (§24).

---

## 7. On-disk shard format & local disk layout

```
<data_dir>/
  volumes/
    <volume-id>/
      VOLUME_META            format_version, volume_id, node_id, created_at
      shards/
        <ab>/<cd>/<shard-id>          content-addressed 2-level hash-prefix dirs (§49)
      tmp/
        <shard-id>.tmp                write staging area, swept by GC on startup
  meta/
    raft_log.redb
    raft_state_machine.redb
  NODE_ID                       generated once, persisted, never derived from IP (§34)
```

`shard_id` is a UUIDv7 minted at write time — not derived from object key (§9, §51: keys
are never trusted as paths). The 2-byte hash prefix directories come from the first two
bytes of the shard_id's hex form, keeping any single directory's fan-out bounded even at
billions of shards (this repo's `loony-rs` sibling project already validated this
pattern at 1B-object scale for its own flat SQLite+FS design; we keep the same directory
fan-out reasoning here, one shard = one file).

**Atomic write protocol** (§50), identical for local writes and remote (RPC-received)
writes:
```
create <shard-id>.tmp in tmp/
stream bytes, computing SHA-256 incrementally
fsync(tmp file)
rename(tmp/<id>.tmp -> shards/<ab>/<cd>/<id>)   // atomic on same filesystem
fsync(parent directory)                          // durability of the rename itself
return ShardReceipt{shard_id, checksum, size}
```
A crash before the rename leaves only an orphan temp file (swept by a startup sweep +
periodic GC, never referenced by any manifest). A crash after the rename but before the
caller records the `ShardReceipt` in a manifest leaves a valid, checksummed, *unreferenced*
shard — a normal input to orphan detection (§64), not a correctness bug.

---

## 8. Erasure coding strategy

**Library: `reed-solomon-simd`** — pure Rust, runtime SIMD dispatch (AVX2/SSSE3/NEON
with scalar fallback), O(n log n), benchmarks ahead of `reed-solomon-erasure` in most
cases. Wrapped entirely behind `ErasureCodec` in `erasure`; nothing outside that crate
imports it directly, so it can be swapped if it stalls or a faster option appears.

**Streaming stripe model:** an object's part is split into fixed-size stripes (default
**8 MiB** logical stripe size, configurable). Each stripe is independently encoded into
`data + parity` shards of `stripe_len / data` bytes each (padded on the final stripe).
The encoder consumes the HTTP body as a `Stream<Bytes>`, buffers only up to one stripe's
worth of bytes at a time (bounded — see §14/memory), encodes it, and immediately starts
concurrent writes of that stripe's shards to their placement targets while the next
stripe is still being read from the client. This is why range GETs and reconstruction
never require the whole object: stripes are the unit of both encoding and I/O.

**N+M semantics:** for `N` data + `M` parity shards, any `N` of the `N+M` shards
reconstruct the stripe; the scheme tolerates the loss of **any `M` shards**. Documented
per configured policy, e.g.:
- `4+2`: tolerates 2 shard losses, 1.5× storage overhead.
- `8+4`: tolerates 4 shard losses, 1.5× storage overhead, better per-object overhead
  ratio for larger objects (more data amortizing the same relative parity cost), worse
  minimum object size before it's worth erasure-coding at all (§9/§17).

Reads normally fetch only `N` shards (whichever `N` targets respond fastest/are healthy)
plus reconstruct only if fewer than `N` original-data shards are directly available —
i.e. if all `N` data shards are healthy, no math is needed at all, we just concatenate
(§22).

---

## 9. Small-object policy

Below a configurable **`small_object_threshold`** (default 512 KiB), objects use
**3-way replication** instead of erasure coding: the whole part (it fits in one stripe
by definition) is written as 3 identical shards to 3 distinct placement targets.
Reconstruction is a straight copy from any surviving replica — no Reed-Solomon math, no
minimum-shard-count math, and no per-object fixed overhead from parity-shard metadata
(a stripe with `N=4,M=2` still needs 6 `ShardLocation` entries and 6 physical files even
for a 40-byte object, which is where naive erasure-coding-everything degenerates).

Tradeoffs, documented for operators:
- Replication: 3× storage overhead (vs 1.5× for 4+2), tolerates 2 losses, trivial CPU
  cost, one write pattern reused by Raft/metadata's own single-node case reasoning.
- Erasure coding: lower overhead at scale, higher CPU (encode/decode), only pays off
  once object size amortizes the fixed per-stripe/shard metadata and I/O overhead.
- Packing small objects into shared containers (SlabDB-style) is **not** implemented in
  the initial release — real complexity (compaction, cross-object GC dependencies) not
  justified until replication's storage overhead is shown to matter for a real workload
  (§17 explicitly permits deferring this).

---

## 10. Placement algorithm

**Rendezvous hashing (HRW)** over the set of healthy `(node_id, volume_id)` pairs,
weighted by available capacity: for `stripe_key = hash(object_id, version_id,
stripe_index)`, each candidate target's score is `hash(stripe_key, node_id, volume_id) *
weight(target)`; the top `N+M` (or `3` for replication) scores are selected, in
descending order, as `shard_index` assignment 0..N+M-1.

Constraints applied before scoring, not after (candidates are filtered, not
scored-then-discarded):
- Only `ACTIVE` volumes on `HEALTHY` nodes are candidates.
- Failure-domain spread: reject a candidate set where two shards of the same stripe
  would land in the same failure domain (initially: same node; the domain hierarchy in
  §19 — rack/zone/region — is modeled in `NodeInfo.failure_domain: Vec<DomainLabel>` from
  day one so this constraint tightens without a data-model change later) *if* enough
  alternative targets exist; if the cluster is too small to satisfy the constraint
  (e.g. a 3-node cluster running 4+2), placement fails fast with a clear
  `InsufficientFailureDomains` error rather than silently co-locating shards.

Rendezvous hashing is chosen over consistent hashing with vnodes because it gives
placement *without any persisted ring state* — the same deterministic function run on
the current healthy-member list reproduces the same target set, and adding/removing a
node only remaps the shards whose top-N+M scores changed (minimal disruption), which is
exactly the property rebalancing (§46) depends on: a placement *generation* is just "the
membership list this decision was computed against," not a separately maintained
structure that itself needs consensus.

---

## 11. Write quorum semantics

Every `DurabilityPolicy` specifies:
- `total_shards` (N+M, or 3 for replication)
- `min_reconstructable` (N, or 1 for replication)
- `write_fault_tolerance` (`wft`, default 1): the number of shard writes allowed to fail
  **during the PUT itself** while still committing.

**Minimum shard writes to commit = `total_shards - wft`**, and this value must always be
strictly greater than `min_reconstructable` (enforced at config-validation time, not at
write time) — i.e. a commit always leaves at least one shard of margin beyond the bare
minimum needed to reconstruct, so the object survives losing one more shard before
healing has a chance to run. For default `4+2` with `wft=1`: 5 of 6 shards must
durably ack; the object then tolerates 1 more loss immediately, up to 2 total once
healing (§32) replaces the missing one.

Metadata commit requirement: **always** — a `CommitManifest` Raft command must reach
Raft quorum (majority of voters) regardless of durability policy. This is a separate,
non-configurable requirement (§39: no writes are accepted metadata-quorum can't be
reached, full stop).

Failure behavior: if fewer than `total_shards - wft` shard writes ack within the
operation timeout, the coordinator aborts — it does **not** attempt a partial commit,
does **not** call `CommitManifest`, and the object remains invisible (§21/§102). Shards
that did get written become orphans, GC'd per §64.

---

## 12–14. Distributed PUT / GET / DELETE — detailed sequences

### PUT (cluster)

```
1.  Client → any node (coordinator): PUT /{bucket}/{key}, streamed body
2.  Coordinator: SigV4 auth, authorize, validate bucket exists (read-index metadata read)
3.  Coordinator: choose DurabilityPolicy (size-based, §9), call PlacementEngine
    → PlacementPlan{ per-stripe target list }
4.  Coordinator: streaming encoder reads body incrementally, stripe by stripe
5.  For each stripe: encode → concurrently PutShard() to all `total_shards` targets
    (local targets via direct disk call, remote via rpc), with bounded in-flight
    stripes (backpressure, §42)
6.  Coordinator waits per-stripe for `>= total_shards - wft` acks; on shortfall, abort
    (goto step 9-abort)
7.  After the last stripe: coordinator has the full PartManifest (and ObjectManifest
    for non-multipart PUTs)
8.  Coordinator issues CommitManifest to the Raft leader (redirected there if the
    coordinator isn't the leader) — this is the atomic visibility point (§21)
9.  On Raft-quorum success: 200 OK + ETag returned to client. Old version (if any)
    becomes a GC candidate, not deleted synchronously.
    On abort/failure: no CommitManifest is ever sent; written shards are unreferenced
    orphans; client receives 5xx and may retry (retry is safe — see idempotency below).
```

**Coordinator-crash analysis, step by step:**
- Crash before step 5 (no shard writes issued): no side effects. Client retries fresh.
- Crash during step 5/6 (some shards written, no CommitManifest sent): object was never
  visible (invariant 2, §78). Written shards are orphans; §64 reclaims them after the
  grace period. Client retry is a completely new attempt with a new `object_id`/upload
  context — never resumes the half-written one, so no dedup logic is needed here.
- Crash after CommitManifest is durably appended to the Raft log but before the
  coordinator returns 200 to the client: the write **did** happen from the system's
  point of view (Raft quorum has it). Client sees a timeout/connection error and may
  retry; retry runs a brand new PUT that creates a new version (if versioning is on) or
  overwrites (if not) — LS3 PUT is idempotent-by-overwrite by design, so a duplicate
  successful write is harmless. This is why PUT does not need an idempotency token the
  way, say, CompleteMultipartUpload does (§44) — LS3's own PUT semantics already tolerate
  it.
- Crash of the Raft *leader* specifically during step 8: the in-flight
  `CommitManifest` proposal is either present in the new leader's log (if it reached
  quorum before the crash — it commits, safe) or absent (client sees failure, retries
  safely per above). No third state is possible; Raft guarantees a proposal is either
  eventually committed or never becomes visible to any future leader.

### GET (cluster, including degraded reads)

```
1.  Client → any node: GET /{bucket}/{key}[?versionId=]
2.  Coordinator: auth/authorize, MetadataStore.get_manifest() (linearizable if the
    request has no versionId pin and freshness matters; §39 read-index)
3.  Determine required stripes from Range header (§24) or whole object
4.  For each stripe: PlacementEngine.targets_for_read() → issue GetShard() to the
    first `min_reconstructable` targets that are known-healthy (per ClusterMembership),
    concurrently, with a short per-shard timeout
5.  Verify each returned shard's checksum against ShardLocation.checksum (§30/§103);
    a failed-checksum shard is treated exactly like an unreachable node — discarded,
    next candidate tried
6.  If `min_reconstructable` data shards were fetched directly: concatenate, no math.
    Else (some data shards missing/corrupt but enough total shards, data+parity,
    available): ErasureCodec.reconstruct()
7.  If fewer than `min_reconstructable` healthy shards exist across the whole target
    set: return 5xx (ServiceUnavailable) — do not block indefinitely (§23) — and enqueue
    a healing job
8.  Stream verified bytes to client as they're reconstructed/read, don't buffer the
    whole object (§4 priority: streaming I/O)
9.  If step 4-6 needed reconstruction (i.e. didn't hit the "no math" fast path),
    enqueue a best-effort healing job for the missing/corrupt shard (§23)
```
No coordinator-crash analysis needed here in the write-safety sense — GET has no
durable side effect to leave half-done; a crashed coordinator simply means the client's
connection drops and it retries against a (possibly different) node.

### DELETE (cluster)

```
1.  Client → any node: DELETE /{bucket}/{key}[?versionId=]
2.  Coordinator: auth/authorize
3.  Coordinator: MetadataStore.tombstone_object() — a Raft command that either creates
    a delete marker (versioning enabled, no versionId given) or marks a specific
    version's manifest as tombstoned (versionId given / versioning disabled)
4.  On Raft-quorum success: 204 No Content. Object/version is now invisible to GET/LIST.
5.  Physical shard deletion is NOT attempted synchronously — it is left entirely to
    background GC (§25/§104) after `tombstone_grace_period` elapses
```
Crash analysis: identical shape to PUT's CommitManifest step — the tombstone command
either reaches Raft quorum (delete is effective, retries are no-ops) or it doesn't (delete
never happened, client retries safely; DELETE is naturally idempotent).

---

## 15. Multipart architecture

- `CreateMultipartUpload` → Raft command allocates `upload_id`, records `(bucket, key,
  initiated_at)` in the state machine. Cluster-visible immediately (any node can see it
  after a read-index read).
- `UploadPart` → coordinator (whichever node receives it, not necessarily the one that
  handled Create) runs the same streaming-encode-and-write pipeline as a normal PUT,
  producing a durable `PartManifest`, then a lightweight Raft command `RecordPart`
  appends it to the upload's part list. Re-uploading the same `part_number` overwrites
  the recorded `PartManifest` (LS3 semantics: parts can be re-uploaded until Complete).
- `ListParts` → pure metadata read.
- `CompleteMultipartUpload` → client supplies the ordered part-number/ETag list; the
  coordinator validates it against `RecordPart` history (§27 "InvalidPartOrder" /
  "InvalidPart" errors, §57), builds the final `ObjectManifest.parts` by concatenation
  (§6), and issues **one** `CommitManifest` Raft command — the atomicity boundary the
  spec asks for (§27) is exactly the same atomic-visibility mechanism as a normal PUT,
  not a separate protocol.
- `AbortMultipartUpload` → Raft command marks the upload aborted; already-written part
  shards become GC candidates after the grace period, same as any other orphan.
- **Idempotency token**: `CompleteMultipart` is not naturally idempotent the way a plain
  PUT is (calling it twice with a part list could, without a token, be interpreted as
  "complete again" against a since-changed part set). Each `CompleteMultipartUpload`
  request carries a client-supplied or coordinator-minted idempotency key stored
  alongside the upload; a retry with the same key against an already-completed upload
  returns the original result instead of re-executing (§44).

## 16. Versioning architecture

`VersioningState ∈ {Disabled, Enabled, Suspended}` stored on the bucket. `version_id`
is a UUIDv7 minted client-side-of-Raft (by the coordinator) at manifest-build time —
time-ordered without needing a sequence counter from the metadata leader, which matters
because it means the expensive streaming/encoding work (steps 4-7 of PUT) never has to
wait on a Raft round-trip; only the final `CommitManifest` does.

- Enabled: every PUT to the same key creates a new version; `latest` pointer updates
  atomically as part of the same `CommitManifest` command.
- Suspended: PUT overwrites the "null" version in place (LS3 semantics) rather than
  creating a new version_id.
- Disabled: single version per key, always overwritten (this is also standalone mode's
  and cluster mode's default — no versioning subsystem to disable, it's the same code
  with `VersioningState::Disabled`).
- `DELETE` without `versionId` on an `Enabled` bucket creates a delete-marker version
  (a manifest-less version entry with `delete_marker=true`) rather than removing
  anything (§26/§104).
- `GET`/`DELETE ?versionId=X` operate on that exact manifest, bypassing the `latest`
  pointer entirely.

---

## 17. Cluster bootstrap / join procedure

```bash
# first node
server --mode cluster --node-id node-01 --bootstrap --cluster-id prod-cluster-1

# subsequent nodes
server --mode cluster --node-id node-03 --join https://node-01:9100
```

```
Joining node:
1.  Load/create persistent NODE_ID (§34) — never derived from network address
2.  Contact --join address over rpc (mTLS handshake, §41)
3.  Present its own node cert + requested role (voter-candidate or storage-only)
4.  Receiving node verifies the joiner's cert was signed by the cluster CA and that
    the joiner's claimed cluster_id (if it has one persisted from a prior life)
    matches this cluster's cluster_id (§36) — refuses a mismatched join outright,
    never silently merges
5.  Receiving node forwards the join request to the current Raft leader
6.  Leader issues RegisterNode (Raft command) — node enters JOINING
7.  If the node was requested/eligible as a voter and the operator explicitly promotes
    it (admin cluster promote node-03): leader runs openraft joint-consensus
    membership change to add it as a learner, waits for it to catch up (log/snapshot
    replication), then promotes learner → voter. Storage-only nodes stay learners
    forever or aren't added to the Raft group at all, but are members of ClusterMembership
    regardless (voter status and cluster-membership status are independent axes)
8.  Node registers its local volumes (RegisterVolume commands) — now eligible as a
    PlacementEngine target
9.  UpdateNodeState JOINING → HEALTHY once it starts responding to heartbeats
```

`--bootstrap` is only valid against an empty data directory and mints a fresh
`cluster_id` (persisted, §36); it is the one place a brand new Raft group with a single
initial voter is created. Every other join is a membership-change against an existing
group — there is no separate "first three nodes special-case" path.

---

## 18. Failure-detection strategy

- Every node sends periodic heartbeats (default 1s) to every other node it currently
  believes is a member, over `rpc`'s `Health` call.
- Each node keeps **local, non-authoritative** hints: consecutive missed heartbeats
  move a peer from `HEALTHY` to a locally-suspected state.
- Authoritative state transitions (the states enumerated in §33 — the ones
  `PlacementEngine` and readiness checks actually consult) only happen via
  `UpdateNodeState` Raft commands, proposed by whichever node currently holds the Raft
  leadership, aggregating hints from `report_health()` calls across the cluster (a
  single node's flaky link doesn't unilaterally declare another node OFFLINE — this
  avoids exactly the gossip-vs-authoritative-state split-brain risk called out in the
  design principle above).
- Hysteresis: `SUSPECT` requires missed heartbeats past a threshold from a **majority**
  of currently-healthy nodes' hints, sustained past a debounce window, before the leader
  proposes `OFFLINE`. A single missed heartbeat never flips authoritative state
  (§33 requirement).
- `DRAINING`/`REMOVED` are always operator-initiated (`admin node drain|remove`),
  never inferred from heartbeats.

---

## 19. Healing algorithm

```
1.  Trigger: degraded-read hint (§14 GET step 9), or periodic healing scanner walking
    manifests looking for ShardLocation entries on OFFLINE nodes/volumes, or scrubber
    finding a checksum mismatch (§20)
2.  Load the current manifest for the affected object/version fresh (not from whatever
    triggered the job — avoid acting on stale data)
3.  Fetch `min_reconstructable` healthy shards for the affected stripe
4.  Reconstruct the missing shard's bytes (ErasureCodec, or plain copy for replication)
5.  PlacementEngine selects a destination target excluding the currently-healthy
    targets for that stripe (so the repaired shard doesn't collide with survivors)
6.  Write the new shard (atomic local-write protocol, §7), get back checksum + new
    shard_id, generation = old_generation + 1
7.  Propose an UpdateManifestShard Raft command: **compare-and-swap** on
    (object_id, version_id, generation) — succeeds only if the manifest's current
    generation for that shard slot still matches what step 2 observed
8.  On CAS success: manifest updated, healing job done. On CAS failure (another healer,
    or a concurrent overwrite/delete, changed the manifest first): discard the newly
    written shard as an orphan (§64) and simply stop — the object is either already
    healed by the winner, or no longer needs healing (deleted/overwritten), either way
    idempotent (§32 requirement: "multiple nodes attempting the same repair must not
    corrupt metadata" — the CAS is the whole mechanism)
```
This same CAS pattern is reused by rebalancing (§20) — "heal" and "move to a new target
because of rebalancing" are the same state machine with a different trigger.

## 20. Rebalancing algorithm

Triggered by `admin node drain <id>` or capacity-change detection. Producing a
`RebalancePlan` is just `PlacementEngine.plan_write()` re-run against the *new* target
membership list for every stripe currently placed (fully or partially) on the
draining/removed node/volume — rendezvous hashing (§10) means only the affected stripes'
top-N+M scores change, so the plan is naturally minimal, not a full reshuffle.
Execution is the *exact same* reconstruct → write-to-new-target → CAS-update-manifest
sequence as healing (§19 steps 3-8), just sourced from a rebalance plan instead of a
detected-missing-shard trigger, and throttled via a configurable bandwidth/concurrency
limit so it never saturates production disks (§31 applies equally here). Node drain
(§47) polls "any stripe still referencing the draining node/volume?" and only reports
drain-complete once the answer is no — it must never tell an operator a node is safe to
remove while doing so would drop any object below `min_reconstructable`.

---

## 21. Garbage collection strategy

Four independent sweeps, all "prove unreferenced, then delete after a grace period"
(§63/§104), never "delete because a normal operation looked done":
1. **Tombstone GC**: `tombstone_object`'d versions past `tombstone_grace_period` →
   delete every `ShardLocation` in that version's manifest, then remove the manifest
   record itself.
2. **Orphan shard GC** (§64): periodic scan of each volume's physical shard IDs vs. a
   Bloom-filter/set snapshot of all `shard_id`s currently referenced by any live
   manifest (built from a metadata scan); a physical shard not in that set **and**
   older than `orphan_grace_period` (protects shards from a write that's mid-flight or
   whose `CommitManifest` hasn't propagated to this reader's snapshot yet) is deleted.
3. **Abandoned multipart GC**: uploads with no `RecordPart`/completion activity past a
   configurable age → treated as `AbortMultipartUpload`, then normal orphan GC reclaims
   the part shards.
4. **Temp-file sweep**: `tmp/*.tmp` older than a short threshold (crash remnants from
   §7's atomic write protocol) — deleted unconditionally, they are never referenced by
   any manifest by construction.

---

## 22. Crash-consistency summary

Every protocol above (PUT/DELETE/multipart-complete/heal/rebalance) shares one shape:
*do all the expensive/risky work first, commit the fact of its completion last, via a
single Raft-quorum'd command, and only that command's success makes anything visible or
final.* A crash anywhere before that command is a no-op from the system's perspective
(possibly leaving orphans, always cleaned up later, never leaving a wrong answer). A
crash anywhere after that command is a normal "client didn't get the response" case that
retries handle. This one shape is why the design doesn't need bespoke two-phase-commit
logic per operation — it's the same pattern instantiated six times.

## 23. Network-partition behavior

- **Leader loss (leader node fails, quorum of remaining voters intact)**: new leader
  elected via standard Raft election (pre-vote reduces disruption from the old leader
  rejoining later with a stale higher term). Brief write unavailability during election
  (bounded by election timeout, default ~300-600ms range with randomized jitter);
  reads relying on the read-index barrier are also blocked until a new leader is
  confirmed, reads not requiring linearizability may continue from local state.
- **Majority partition** (the side with quorum): continues accepting writes normally.
- **Minority partition** (the side without quorum): its local Raft role can never
  become/stay leader; `MetadataStore.commit_manifest`/any write command fails fast with
  a clear `MetadataUnavailable` error rather than hanging; `/health/ready` on those
  nodes reports not-ready (§68) so a load balancer stops routing writes there. Reads of
  already-known local data may be served **only** in an explicitly configured
  stale-read mode (default: off — reads also go through the read-index barrier and thus
  also fail on a minority node, favoring correctness over availability, consistent with
  priority order in §0: correctness > durability > everything else).
- **Network split with no majority on either side** (e.g. a 3-way even split): no side
  elects a leader; the entire metadata plane is unavailable for writes and default-mode
  reads until enough connectivity returns to form a majority somewhere. This is the
  correct, boring CP behavior — never split-brain metadata commits (§39/invariant 6).

---

## 24. Security architecture

- **LS3-facing auth**: SigV4 (headers + presigned URLs) in `auth`, constant-time
  (`subtle`) signature comparison, credential secrets stored **encrypted at rest**
  (envelope-encrypted with a node-local/KMS-provided key) rather than hashed — SigV4
  requires deriving an HMAC signing key from the actual secret server-side, so a
  one-way hash (appropriate for passwords) cannot work here; this is called out
  explicitly because it's a common and dangerous mistake to copy password-hashing
  practice onto SigV4 secrets.
- **Internal RPC auth**: mutual TLS. Cluster bootstrap mints a cluster-local CA (dev/
  test default); production deployments may supply their own CA/certs. Node identity
  is the cert's subject, checked against the persisted `node_id`/`cluster_id`, never
  trust-by-source-IP (§41 explicit requirement).
- **Authorization**: separate from authentication (§55) — initial release is
  ownership/root-style (a credential's `owner_id` must match the bucket/object owner,
  or be the configured root principal); `auth` exposes an `Authorizer` trait so an
  IAM-policy-style engine can be added later without touching `api`.
- **Filesystem/key safety** (§51): object keys are validated (UTF-8, length, reject
  control chars/null bytes) and are **never** used to construct a physical path;
  physical paths are always `shard_id`-derived (§7), so path traversal via a malicious
  key is structurally impossible, not just filtered.
- **Resource limits** (§65) are enforced at the `api` boundary before any
  domain-service work starts: header count/size, max object size, max metadata size,
  max multipart parts, concurrent request/upload/download caps.
- Certificate rotation: internal certs carry short validity + a rotation admin op that
  performs a rolling per-node cert swap without a full cluster restart (§84/§89).

---

## 25. Testing & fault-injection strategy

Mirrors §76-79 directly, organized so each layer's tests don't need the layers above it:
- **Unit**: `erasure` (property test: encode random data, drop up to M shards,
  reconstruct, assert equality — §79), `placement` (determinism + failure-domain
  constraint property tests), `auth` (official SigV4 test vectors), range-header
  parser, manifest (de)serialization round-trips.
- **Component**: `metadata` against a real (single-node) openraft+redb instance —
  state-machine transition tests, snapshot/restore round-trip.
- **Integration**: standalone `server` process + standard clients/SDKs against it — full
  LS3 API surface (§75/§99, SHA-256 round-trip after every listed scenario).
- **Cluster/failure tests** (§77): a test harness spins up N `server` processes
  (or in-process tasks with an injectable `rpc` transport for faster iteration) and
  drives exactly the scenario list in §77 — kill during PUT, kill during GET, corrupt a
  shard, partition the leader, duplicate/delay an RPC (via the injectable transport),
  restart after long outage, concurrent heal-vs-delete, etc. — asserting the §78
  invariants after each.
- **Benchmarks** (§80): Criterion benches for encode/decode CPU, and a small
  harness-driven throughput bench for PUT/GET across object sizes 1 KiB→1 GiB+, run
  outside CI on demand, never with generated fixtures checked into git (§80).

---

## 26. Implementation phases

Phases 0-15 as listed in the prompt's own §90 are adopted as-is, with one refinement now
that §2's unification decision is locked in: **Phase 2 (metadata state machine)
implements the openraft+redb state machine directly, single-voter, from the start** —
there is no separate "local-only" metadata implementation to later replace with Raft in
Phase 8. Phase 8 becomes "multi-voter Raft membership changes + snapshot transfer +
partition/leader-failure tests," not "introduce Raft." This removes a rewrite that the
original phase list's SQLite-then-Raft framing would otherwise require.

---

## 27. Key technical risks & mitigations

| Risk | Mitigation |
|---|---|
| `openraft` is pre-1.0; breaking API changes between releases | Entirely wrapped behind `MetadataStore`; pin an exact version in `Cargo.lock`; upgrade is a `metadata`-internal change |
| Erasure reconstruction CPU cost under concurrent degraded reads | Default read path avoids reconstruction entirely when all N data shards are healthy (§8); reconstruction concurrency is bounded/configurable, same as healing |
| Small-object-heavy workloads bottlenecking the Raft metadata log (every PUT is a commit) | Small-object replication policy avoids erasure-coding overhead; batching multiple `CommitManifest` proposals per Raft round where the client workload allows is a documented future optimization, not required for correctness |
| Operational complexity of running Raft correctly | Small fixed voter set (3/5) decoupled from storage-node count; extensive §77-style failure tests before any release is called done |
| Cross-node clock skew | All authoritative ordering (versions, commits) comes from Raft log order or UUIDv7 minted locally for uniqueness, never wall-clock comparison across nodes for correctness decisions; wall-clock timestamps are informational only |
| Rendezvous-hash placement + small cluster can't satisfy failure-domain spread | Placement fails fast with a specific error (§10) instead of silently weakening durability |
| `reed-solomon-simd` or `redb` stagnate/are abandoned | Both isolated behind traits (`ErasureCodec`, and redb only touched inside `metadata`/`storage`'s volume-meta handling) — swappable without touching call sites |

---

## 28. Explicit system invariants

Adopting §78's seven invariants verbatim as the base set, plus the ones this design adds
specifically:

1. A committed object must never reference insufficient durable data (`total_shards -
   wft` were durably written before `CommitManifest`, §11).
2. An uncommitted object must never become visible (no `CommitManifest` ⇒ no visibility,
   §21).
3. A stale manifest must never replace a newer committed generation (Raft's log order is
   the single source of truth for "newer"; healing/rebalancing use per-shard CAS keyed
   on `generation`, §19, so even sub-object updates respect this).
4. GC must never delete data referenced by a live manifest (orphan GC diffs against a
   fresh live-manifest scan + grace period, §21).
5. Healing must preserve object contents exactly (reconstruction is verified against the
   stripe's own checksums before the CAS write, §19 step 6 checksum, §30).
6. Minority partitions must not create conflicting metadata commits (no leader ⇒ no
   commits possible on that side, §23).
7. Object bytes returned to a client must pass integrity verification (§30/§103 — a
   failed-checksum shard is never served, always substituted/reconstructed).
8. **(added)** A Raft voter-set change never risks a split quorum (joint consensus only,
   §17/§5).
9. **(added)** Two shards of the same stripe are never placed in the same failure domain
   when the cluster topology makes avoiding it possible (§10); when it isn't possible,
   the write is refused, not silently weakened.
10. **(added)** A physical shard write is never treated as durable for quorum purposes
    (§11) until its bytes are fsync'd and the rename in §7's atomic-write protocol has
    completed — a `ShardReceipt` is only returned after that.

---

## Open items intentionally deferred (not blocking Phase 1)

- Exact Raft snapshot cadence/thresholds — tunable, default chosen empirically once
  Phase 8 has a running multi-voter cluster to measure against.
- Small-object packing (§9) — deferred per §17's explicit permission to do so.
- IAM-policy-style authorization engine — trait exists (`Authorizer`), no implementation
  beyond ownership/root yet (§55/§100 non-goal for initial release).
- Cross-region replication, SQL-over-object queries, archival storage tiers — explicit non-goals (§100).
