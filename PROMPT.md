# ROLE

Act as a principal Rust engineer, distributed-systems architect, storage-engine engineer, and S3 compatibility specialist.

Build a production-oriented **S3-compatible object storage system in Rust**.

This is NOT an AWS S3 client.

We are building our **own object-storage server**, similar in purpose to systems such as MinIO, that exposes an AWS S3-compatible API.

The system MUST support two deployment modes from the same codebase:

```bash
server --mode standalone
```

and:

```bash
server --mode cluster
```

Distribution MUST NOT be bolted on after implementing the standalone version.

The architecture must be designed from day one so that:

* standalone mode uses the same storage concepts as cluster mode
* cluster mode supports multiple storage nodes
* objects can survive node/disk failures according to configured durability policy
* cluster operations do not depend on shared local filesystems
* APIs behave consistently between standalone and cluster modes

Primary engineering priorities:

1. Correctness
2. Data durability
3. S3 compatibility
4. Streaming I/O
5. Distributed correctness
6. Security
7. Failure recovery
8. Horizontal scalability
9. Observability
10. Performance
11. Maintainable Rust architecture
12. Testability

Do not fake distributed behavior.

Do not implement the entire system in one file.

Do not use an architecture where cluster mode is merely multiple HTTP servers pointing at the same local filesystem.

---

# 1. TECHNOLOGY STACK

Use stable Rust, edition 2024.

Core stack:

```text
Tokio
Axum
Hyper
Tower
Tower HTTP
Serde
Bytes
Futures
Tracing
tracing-subscriber
thiserror
uuid
time
sha2
hmac
subtle
quick-xml
```

For erasure coding, either use a mature Rust Reed-Solomon implementation or implement an abstraction around one.

For metadata persistence, choose an architecture appropriate for both deployment modes.

Standalone mode may use:

```text
SQLite / embedded transactional database
```

or another justified embedded metadata store.

Cluster mode MUST NOT rely on an ordinary single PostgreSQL instance as the sole source of distributed coordination.

Choose and justify one of:

```text
Raft-based metadata consensus
distributed transactional metadata
embedded replicated state machine
another strongly justified design
```

Prefer a Raft-based replicated metadata state machine if practical.

Potential Rust ecosystem choices may include:

```text
openraft
rocksdb/redb/sled-equivalent embedded persistence
```

but verify current crate maturity before selecting dependencies.

Do not blindly add dependencies.

Explain why each important dependency exists.

---

# 2. CORE PRODUCT REQUIREMENT

One codebase must support:

```text
STANDALONE MODE
```

and:

```text
CLUSTER MODE
```

The S3 layer should not care whether the underlying system is one node or many nodes.

Use abstractions around:

```text
metadata
object placement
shard storage
cluster membership
durability
```

rather than branching throughout every S3 handler.

---

# 3. STANDALONE MODE

Standalone mode runs everything on one machine.

Architecture:

```text
                   S3 Clients
                       │
                       ▼
              ┌─────────────────┐
              │    Rust Node    │
              │                 │
              │     S3 API      │
              │      SigV4      │
              │ Object Service  │
              │    Metadata     │
              │ Storage Engine  │
              └────────┬────────┘
                       │
             ┌─────────┼─────────┐
             ▼         ▼         ▼
           Disk 1    Disk 2    Disk 3
```

Standalone mode must support:

* one disk
* multiple disks
* normal object operations
* multipart uploads
* versioning
* checksums
* background garbage collection
* optional erasure coding across local disks

A one-disk standalone configuration should remain possible for development.

---

# 4. CLUSTER MODE

Cluster mode consists of multiple Rust nodes.

Example:

```text
                         S3 Clients
                              │
                              ▼
                       Load Balancer
                              │
             ┌────────────────┼────────────────┐
             ▼                ▼                ▼
         Rust Node A      Rust Node B      Rust Node C
             │                │                │
             └────────────────┼────────────────┘
                              │
                  Distributed Metadata
                    / Cluster Control
                              │
               ┌──────────────┼──────────────┐
               ▼              ▼              ▼
             Node A         Node B         Node C
           ┌───┴───┐      ┌───┴───┐      ┌───┴───┐
          Disk    Disk    Disk    Disk    Disk    Disk
```

Any API node should be capable of receiving an S3 request.

The receiving node coordinates the operation.

Do not require clients to know which node physically contains an object.

---

# 5. MAJOR SUBSYSTEMS

Design explicit modules for:

```text
S3 API
Authentication
Authorization
Bucket Service
Object Service
Multipart Service
Versioning

Cluster Membership
Node Discovery
Failure Detection
Distributed Metadata
Consensus
Placement
Quorum

Erasure Coding
Shard Storage
Object Manifest
Checksums

Healing
Scrubbing
Garbage Collection
Rebalancing

Local Disk Engine

Metrics
Tracing
Health
Administration
```

Avoid tight coupling between these systems.

---

# 6. SUGGESTED WORKSPACE

Prefer a Cargo workspace.

For example:

```text
rust-s3/
├── Cargo.toml
├── crates/
│   ├── server/
│   ├── api/
│   ├── auth/
│   ├── core/
│   ├── metadata/
│   ├── cluster/
│   ├── placement/
│   ├── erasure/
│   ├── storage/
│   ├── healing/
│   ├── admin/
│   └── observability/
│
├── migrations/
├── tests/
├── benches/
├── docs/
├── Dockerfile
├── docker-compose.yml
└── README.md
```

Do not split crates merely for appearance.

Each crate must have a clear responsibility.

---

# 7. REQUEST ARCHITECTURE

S3 requests should conceptually follow:

```text
HTTP
 │
 ▼
S3 Router
 │
 ▼
SigV4 Authentication
 │
 ▼
Authorization
 │
 ▼
S3 Operation
 │
 ▼
Domain Service
 │
 ├──── Metadata
 │
 └──── Object Data
          │
          ▼
      Placement
          │
          ▼
    Durability Layer
          │
     ┌────┴─────┐
     │          │
Replication   Erasure Coding
     │          │
     └────┬─────┘
          ▼
      Shard I/O
          │
          ▼
   Local/Remote Nodes
```

The HTTP handlers should contain very little business logic.

---

# 8. BUCKET MODEL

Bucket metadata should contain at minimum:

```text
bucket_id
name
owner_id
creation_time
region
versioning_state
quota
status
```

Bucket names must be globally unique within a cluster.

Validate S3-style bucket naming rules.

Bucket creation/deletion must be cluster-consistent.

---

# 9. OBJECT MODEL

Objects need logical metadata independent from their physical shards.

Example:

```text
object_id
bucket_id
key
version_id
size
etag
sha256
content_type
content_encoding
cache_control
content_disposition
user_metadata
created_at
modified_at
delete_marker
manifest_id
```

Object keys are arbitrary logical identifiers.

Example:

```text
users/123/avatar.jpg
videos/2026/movie.mp4
backups/mysql/db.sql
```

Never treat them as trusted filesystem paths.

---

# 10. OBJECT MANIFEST

Every committed object should have a manifest describing its physical representation.

Example conceptual structure:

```rust
ObjectManifest {
    object_id,
    version_id,
    size,
    checksum,
    erasure_scheme,
    data_shards,
    parity_shards,
    shards: Vec<ShardLocation>,
}
```

A shard location should contain information such as:

```text
shard_index
node_id
volume_id
physical_id
size
checksum
generation
```

The manifest is authoritative metadata.

Do not expose internal physical paths through S3 APIs.

---

# 11. STORAGE ABSTRACTIONS

Define clean interfaces.

Conceptually:

```rust
trait MetadataStore {
    // bucket/object/manifest operations
}

trait ShardStore {
    // put/get/delete shard
}

trait PlacementEngine {
    // select nodes/volumes
}

trait ClusterMembership {
    // nodes and health
}

trait ErasureCodec {
    // encode/reconstruct
}
```

Actual signatures should be designed idiomatically.

Do not create giant god traits.

---

# 12. CREATE BUCKET

Support:

```http
PUT /{bucket}
```

The request flow in cluster mode should be approximately:

```text
Authenticate
    ↓
Validate bucket
    ↓
Check distributed metadata
    ↓
Consensus operation
    ↓
Commit bucket metadata
    ↓
Return success
```

Concurrent attempts to create the same bucket must resolve consistently.

---

# 13. BUCKET OPERATIONS

Implement:

```text
CreateBucket
DeleteBucket
HeadBucket
ListBuckets
ListObjectsV2
```

ListObjectsV2 must support:

```text
prefix
delimiter
continuation-token
start-after
max-keys
CommonPrefixes
```

Pagination must occur at the metadata layer.

Do not load all object keys into memory.

---

# 14. PUT OBJECT — STANDALONE

Standalone writes must stream.

Conceptual flow:

```text
HTTP Body
   │
   ▼
Streaming reader
   │
   ├── SHA-256
   ├── ETag calculation
   └── size accounting
   │
   ▼
Durability encoder
   │
   ▼
local disk(s)
   │
   ▼
atomic physical commit
   │
   ▼
metadata commit
   │
   ▼
200 OK
```

Never buffer arbitrary objects entirely in RAM.

---

# 15. PUT OBJECT — CLUSTER

Cluster uploads must also stream.

Conceptual flow:

```text
                       PUT object
                           │
                           ▼
                     Coordinator
                           │
                           ▼
                    Placement Plan
                           │
                           ▼
                 Streaming Encoder
                           │
        ┌──────────────────┼──────────────────┐
        ▼                  ▼                  ▼
     shard 0            shard 1            shard N
        │                  │                  │
        ▼                  ▼                  ▼
     Node A             Node B             Node C
        │                  │                  │
        ▼                  ▼                  ▼
     Volume             Volume             Volume
```

The coordinator must not first write the complete object locally before distributing it unless explicitly configured to do so.

Use bounded buffers and backpressure.

---

# 16. ERASURE CODING

Implement configurable Reed-Solomon-style erasure coding.

Example configurations:

```text
4 data + 2 parity
8 data + 4 parity
```

Do not hard-code one configuration.

For an:

```text
N data + M parity
```

scheme, document exactly how many shard failures can be tolerated.

Objects must be split into stripes/chunks suitable for streaming.

Do not require loading an entire object into memory before encoding.

Store checksums independently for each shard.

---

# 17. SMALL OBJECT POLICY

Millions of tiny objects can make naive erasure coding inefficient.

Design an explicit small-object strategy.

Possible approaches include:

```text
replication below threshold
erasure coding above threshold
packing small objects
```

Do not prematurely implement complicated packing unless justified.

At minimum support configurable policy such as:

```text
object < threshold
    → replication

object >= threshold
    → erasure coding
```

Document the tradeoffs.

---

# 18. PLACEMENT ENGINE

Implement deterministic placement.

Placement must consider:

```text
node health
volume health
available capacity
failure domains
existing shard locations
```

Do not place multiple shards of the same stripe on the same disk when alternatives exist.

Prefer different nodes for independent shards.

Design for future topology awareness:

```text
host
rack
availability zone
region
```

A useful algorithm may use rendezvous hashing or another justified deterministic placement method.

---

# 19. FAILURE DOMAINS

Represent:

```text
cluster
region
zone
rack
host
volume
```

even if the initial implementation only actively uses:

```text
host
volume
```

Placement policy should eventually allow constraints such as:

```text
never put two parity/data shards from the same stripe on one disk
```

and preferably:

```text
spread shards across nodes
```

---

# 20. WRITE QUORUM

Define explicit write-success semantics.

A PUT must not return success merely because one node accepted data.

For each durability policy, specify:

```text
minimum shard writes
metadata commit requirement
failure behavior
```

Do not invent quorum formulas without reasoning about recoverability.

An object must not become visible until enough durable shards exist to satisfy the configured commit policy.

---

# 21. ATOMIC VISIBILITY

Readers must see:

```text
old committed object
```

or:

```text
new committed object
```

but never a partially written replacement.

Use generations/manifests.

Conceptually:

```text
write new shards
       ↓
verify durability
       ↓
create new manifest
       ↓
atomic metadata commit
       ↓
new version becomes visible
       ↓
old physical data becomes GC candidate
```

---

# 22. GET OBJECT

GET should work regardless of which API node receives the request.

Flow:

```text
GET object
    │
    ▼
lookup manifest
    │
    ▼
determine required shards
    │
    ▼
fetch shards concurrently
    │
    ▼
verify checksums
    │
    ▼
decode/reconstruct if required
    │
    ▼
stream HTTP response
```

Avoid unnecessary shard reads.

For an N+M erasure scheme, normally read enough healthy shards to reconstruct the object, rather than always reading all N+M.

---

# 23. DEGRADED READS

Suppose:

```text
Node B = offline
```

but sufficient shards remain.

GET must still succeed.

The system should:

```text
detect unavailable shard
      ↓
obtain alternative shards
      ↓
reconstruct missing data
      ↓
serve object
      ↓
optionally enqueue healing
```

A degraded read should not block indefinitely waiting for an offline node.

---

# 24. RANGE REQUESTS

Support:

```http
Range: bytes=...
```

At minimum implement single-range requests.

Return correct:

```text
206 Partial Content
Content-Range
Content-Length
Accept-Ranges
```

Design the erasure layout so range GETs do not require reconstructing the entire object unnecessarily.

Map logical byte ranges to relevant stripes.

---

# 25. DELETE OBJECT

Deletes must be metadata-safe.

Do not synchronously require every shard deletion before acknowledging the logical deletion.

Conceptual flow:

```text
DELETE
   ↓
metadata transaction
   ↓
delete marker / tombstone
   ↓
object becomes invisible
   ↓
background physical GC
```

This avoids failed physical cleanup resurrecting an object.

---

# 26. VERSIONING

Support:

```text
Disabled
Enabled
Suspended
```

When enabled:

```text
PUT same key
```

creates a new version.

Support:

```text
GET ?versionId=
DELETE ?versionId=
```

Normal DELETE should create an appropriate delete marker.

Each version has its own manifest.

---

# 27. MULTIPART UPLOADS

Implement:

```text
CreateMultipartUpload
UploadPart
ListParts
CompleteMultipartUpload
AbortMultipartUpload
```

Each uploaded part must itself be durable according to the multipart durability policy.

Multipart state must be cluster-visible.

Any API node should be able to continue an upload initiated through another node.

Completion must be atomic from the S3 client's perspective.

---

# 28. COPY OBJECT

Implement CopyObject.

Avoid downloading the complete source object into memory.

If possible, optimize internal copies.

Initially it is acceptable to:

```text
stream source
→ normal object write pipeline
```

while maintaining bounded memory.

---

# 29. ETAGS

Implement deterministic ETag behavior.

Do not claim:

```text
ETag == MD5
```

universally.

Multipart ETags require special handling.

Document compatibility differences from AWS S3.

Store cryptographic integrity checksums separately.

---

# 30. SHARD CHECKSUMS

Every shard must have an integrity checksum.

On reads:

```text
read shard
   ↓
verify checksum
   ↓
if invalid
       ↓
treat shard as corrupt
       ↓
reconstruct from healthy shards
       ↓
enqueue repair
```

Never silently serve corrupted bytes.

---

# 31. BIT-ROT PROTECTION

Implement background scrubbing.

Scrubber:

```text
select shard
    ↓
read shard
    ↓
calculate checksum
    ↓
compare stored checksum
    ↓
healthy?
 ┌────┴─────┐
 yes        no
 │           │
done       heal
```

Scrubbing rate must be configurable.

It must not saturate production disks.

---

# 32. HEALING

Create a dedicated healing subsystem.

Healing handles:

```text
missing shard
corrupt shard
replaced disk
rejoined node
incomplete placement
```

Flow:

```text
identify damaged object
       ↓
load manifest
       ↓
find healthy shards
       ↓
reconstruct
       ↓
choose destination
       ↓
write repaired shard
       ↓
verify checksum
       ↓
update manifest safely
```

Healing must be idempotent.

Multiple nodes attempting the same repair must not corrupt metadata.

---

# 33. NODE FAILURE

Nodes exchange health information.

Track states such as:

```text
JOINING
HEALTHY
SUSPECT
OFFLINE
DRAINING
REMOVED
```

Do not immediately declare a node permanently lost because one heartbeat failed.

Distinguish temporary unavailability from administrative removal.

---

# 34. CLUSTER MEMBERSHIP

Every node gets a persistent:

```text
node_id
```

Do not use IP addresses as permanent node identities.

Store:

```text
node_id
advertised_address
cluster_address
state
generation
last_seen
failure_domain
```

Node IDs must survive restart.

---

# 35. NODE DISCOVERY

Support explicit bootstrap configuration.

Example:

```bash
server \
  --mode cluster \
  --node-id node-03 \
  --join http://node-01:9100
```

A joining node must:

```text
contact existing member
      ↓
authenticate
      ↓
obtain cluster identity
      ↓
join metadata consensus
      ↓
register storage volumes
      ↓
transition JOINING → HEALTHY
```

Prevent accidental joining of the wrong cluster.

---

# 36. CLUSTER IDENTITY

Each cluster must have a unique:

```text
cluster_id
```

Persist it.

Nodes should refuse to silently merge unrelated clusters.

---

# 37. DISTRIBUTED METADATA

Cluster metadata includes:

```text
buckets
objects
versions
manifests
multipart uploads
credentials
nodes
volumes
cluster configuration
placement generations
tombstones
```

Metadata must survive node failures.

Use consensus for operations requiring a single authoritative order.

---

# 38. RAFT / CONSENSUS

Prefer a Raft-based metadata architecture.

Clearly separate:

```text
control-plane metadata
```

from:

```text
object data
```

Do NOT put object bodies or erasure shards through Raft.

Raft should replicate small metadata/state-machine commands.

Examples:

```text
CreateBucket
DeleteBucket
CommitObjectManifest
DeleteObject
CompleteMultipart
RegisterNode
UpdateClusterConfig
```

Explain:

```text
leader election
quorum
log replication
snapshotting
recovery
membership changes
```

Do not implement a custom consensus algorithm casually.

Use a mature Rust implementation when appropriate.

---

# 39. CONSENSUS UNAVAILABILITY

If metadata quorum is unavailable, the system must fail safely.

Do not accept writes that cannot be safely committed.

Where appropriate, reads of already-known immutable data may remain possible, but only if consistency semantics are explicitly defined.

Document behavior during:

```text
leader loss
minority partition
majority partition
network split
```

Never allow split-brain metadata commits.

---

# 40. INTERNAL NODE RPC

Create a separate internal protocol for node-to-node communication.

Possible choices:

```text
HTTP/2
gRPC
custom framed protocol
```

Justify the choice.

Internal APIs include:

```text
PutShard
GetShard
DeleteShard
StatShard
Health
ClusterJoin
Healing operations
```

S3 clients must never access these APIs directly.

---

# 41. INTERNAL AUTHENTICATION

Node-to-node communication must be authenticated.

Design for:

```text
mTLS
cluster certificates
node identity
certificate rotation
```

Development mode may support simpler credentials, but production cluster mode must have a secure design.

Do not trust a request merely because it originates from a private IP.

---

# 42. BACKPRESSURE

Uploads must have bounded queues.

Never allow:

```text
client faster than disks
```

to cause unlimited RAM growth.

Flow control must propagate:

```text
disk/network slowdown
        ↓
shard writer
        ↓
encoder
        ↓
HTTP body reader
        ↓
client TCP flow control
```

---

# 43. RETRIES

Internal retries must be bounded.

Use:

```text
timeouts
retry budgets
exponential backoff
jitter
```

Do not retry non-idempotent operations blindly.

Use operation IDs/idempotency tokens where required.

---

# 44. IDEMPOTENCY

Distributed operations must tolerate:

```text
lost responses
retries
coordinator crashes
duplicate RPC delivery
```

A retried shard write must not produce uncontrolled duplicate state.

Use stable operation/generation IDs.

---

# 45. COORDINATOR FAILURE

Explicitly handle:

```text
client → Node A
Node A distributes shards
Node A crashes before replying
```

After restart/retry, the system must determine whether:

```text
operation committed
operation incomplete
operation aborted
```

Do not leave ambiguous externally visible objects.

---

# 46. REBALANCING

When capacity changes:

```text
add node
add disk
remove node
drain node
```

support background rebalancing.

Do not synchronously move the entire cluster.

Use throttled jobs.

Placement generations should allow old and new layouts to coexist safely during migration.

---

# 47. NODE DRAIN

Support:

```bash
admin node drain node-03
```

Draining means:

```text
stop new placement
      ↓
move required shards
      ↓
verify durability
      ↓
mark safe for removal
```

Never tell an administrator a node is safe to remove while its removal would violate configured durability.

---

# 48. DISK MANAGEMENT

Volumes have states:

```text
ACTIVE
READ_ONLY
DRAINING
OFFLINE
FAILED
```

Each volume should have:

```text
volume_id
node_id
path
capacity
used
available
state
```

Use persistent volume IDs.

Do not identify a disk solely by mount path.

---

# 49. LOCAL DISK LAYOUT

Never use raw object keys as physical paths.

Use internal identifiers.

Example:

```text
/data/
  volumes/
    <volume-id>/
      shards/
        ab/
          cd/
            <shard-id>
```

Use hash-prefix directories to avoid enormous flat directories.

---

# 50. ATOMIC LOCAL SHARD WRITE

Shard writes:

```text
create temp file
    ↓
stream bytes
    ↓
checksum
    ↓
flush
    ↓
fsync where required
    ↓
atomic rename
    ↓
persist metadata
```

Handle crashes between each stage.

Temporary files must be garbage collectible.

---

# 51. FILESYSTEM SECURITY

Protect against:

```text
../ traversal
absolute paths
symlink traversal
null bytes
malicious object keys
unexpected file replacement
```

Object keys must never control physical paths.

---

# 52. AWS SIGNATURE V4

Implement AWS Signature Version 4.

Support:

```text
Authorization: AWS4-HMAC-SHA256 ...
```

Correctly implement:

```text
canonical URI
canonical query string
canonical headers
signed headers
payload hash
credential scope
string-to-sign
derived signing key
signature verification
```

Support:

```text
x-amz-date
x-amz-content-sha256
host
```

Use constant-time signature comparison.

Test against official AWS SigV4 vectors.

---

# 53. PRESIGNED URLS

Implement SigV4 presigned:

```text
GET
PUT
```

Validate:

```text
X-Amz-Algorithm
X-Amz-Credential
X-Amz-Date
X-Amz-Expires
X-Amz-SignedHeaders
X-Amz-Signature
```

Enforce expiration.

---

# 54. CREDENTIALS

Create credential records containing:

```text
access_key
secret representation
owner_id
enabled
created_at
```

Never return secret credentials through normal APIs.

Never log secrets.

Design credential lookup through a trait.

---

# 55. AUTHORIZATION

Authentication and authorization must be separate.

Authentication answers:

```text
Who are you?
```

Authorization answers:

```text
May you perform this S3 operation?
```

Initially implement ownership/root-style authorization.

Design policy interfaces so IAM-like policies can be added later.

---

# 56. S3 ENDPOINTS

Implement at minimum:

```text
PUT    /{bucket}
DELETE /{bucket}
HEAD   /{bucket}

GET    /
GET    /{bucket}

PUT    /{bucket}/{key...}
GET    /{bucket}/{key...}
HEAD   /{bucket}/{key...}
DELETE /{bucket}/{key...}
```

plus multipart query variants.

---

# 57. S3 ERROR MODEL

Support errors such as:

```text
NoSuchBucket
NoSuchKey
BucketAlreadyExists
BucketNotEmpty
InvalidBucketName
InvalidArgument
InvalidRange
AccessDenied
SignatureDoesNotMatch
InvalidAccessKeyId
EntityTooLarge
InvalidPart
InvalidPartOrder
NoSuchUpload
SlowDown
ServiceUnavailable
InternalError
```

Return S3-compatible XML.

Example:

```xml
<Error>
    <Code>NoSuchKey</Code>
    <Message>The specified key does not exist.</Message>
    <Key>photo.jpg</Key>
    <RequestId>...</RequestId>
</Error>
```

Never expose internal filesystem paths, cluster secrets, SQL statements, consensus internals, or stack traces.

---

# 58. REQUEST IDs

Every request receives:

```text
request_id
```

Return it in response headers.

Propagate it into:

```text
logs
internal RPC
storage operations
metrics/traces
```

Also use a separate distributed trace ID when appropriate.

---

# 59. LIST OBJECTS V2

Implement correct handling of:

```text
prefix
delimiter
max-keys
continuation-token
start-after
encoding-type
```

Return:

```text
Contents
CommonPrefixes
IsTruncated
NextContinuationToken
```

Continuation tokens must be opaque and validated.

---

# 60. METADATA HEADERS

Support:

```text
Content-Type
Content-Length
Content-Encoding
Content-Disposition
Cache-Control
Content-Language
Expires
x-amz-meta-*
```

Set reasonable limits on user metadata size.

---

# 61. CONDITIONAL REQUESTS

Where practical support:

```text
If-Match
If-None-Match
If-Modified-Since
If-Unmodified-Since
```

Apply correct semantics before streaming large object bodies.

---

# 62. CONCURRENCY

Explicitly test races:

```text
PUT A vs PUT A

PUT A vs DELETE A

GET A vs DELETE A

CompleteMultipart vs AbortMultipart

DeleteBucket vs PutObject

heal vs delete

heal vs overwrite

rebalance vs delete

node drain vs PUT
```

Do not use a global mutex.

Prefer metadata transactions/generations and consensus ordering.

---

# 63. GARBAGE COLLECTION

GC handles:

```text
temporary shards
abandoned uploads
aborted multipart uploads
unreferenced manifests
old object generations
deleted object shards
failed rebalance remnants
orphan shards
```

GC must prove that data is no longer referenced before deletion.

Use grace periods where necessary.

---

# 64. ORPHAN DETECTION

Crashes can create physical shards with no committed manifest.

Implement reconciliation:

```text
scan shard metadata
       ↓
compare with committed manifests
       ↓
respect grace period
       ↓
delete confirmed orphan
```

Never delete unknown data immediately.

---

# 65. RESOURCE LIMITS

Make configurable:

```text
maximum object size
maximum metadata size
maximum headers
maximum multipart parts
multipart part size
active requests
active uploads
active downloads
internal RPC concurrency
healing concurrency
rebalance bandwidth
scrub bandwidth
disk queue depth
request timeout
```

---

# 66. OBSERVABILITY

Use structured tracing.

Include:

```text
request_id
trace_id
node_id
operation
bucket
object key when safe
status
latency
bytes
shard count
degraded status
```

Never log:

```text
Authorization
secret keys
presigned signatures
object bodies
private credentials
```

---

# 67. METRICS

Expose Prometheus-compatible metrics.

Examples:

```text
requests_total
request_duration_seconds
bytes_uploaded_total
bytes_downloaded_total

cluster_nodes
cluster_nodes_offline

storage_capacity_bytes
storage_used_bytes

shard_reads_total
shard_writes_total
shard_corruption_total

healing_jobs_total
healing_bytes_total

rebalance_jobs_total
rebalance_bytes_total

raft_term
raft_commit_index
raft_leader_changes_total

multipart_uploads_active
```

Avoid unbounded metric labels such as object keys.

---

# 68. HEALTH ENDPOINTS

Expose:

```text
/health/live
/health/ready
```

Liveness:

```text
process is alive
```

Readiness considers:

```text
metadata availability
cluster membership
usable volumes
required quorum
```

A minority-partitioned node should not falsely advertise write readiness.

---

# 69. ADMIN API

Create a separate administrative API.

Operations include:

```text
cluster status
node list
volume list
node drain
node remove
healing status
rebalance status
cluster configuration
```

Do not mix privileged administration casually with public S3 endpoints.

---

# 70. ADMIN CLI

Create:

```text
admin
```

Example:

```bash
admin cluster status

admin node list

admin node drain node-03

admin volume list

admin heal status
```

Use authenticated admin communication.

---

# 71. CONFIGURATION

Standalone example:

```env
MODE=standalone
NODE_ID=node-01
BIND_ADDR=0.0.0.0:9000
ADMIN_ADDR=0.0.0.0:9001
DATA_DIR=/data
REGION=us-east-1
```

Cluster example:

```env
MODE=cluster

CLUSTER_ID=cluster-production
NODE_ID=node-01

PUBLIC_ADDR=0.0.0.0:9000
CLUSTER_ADDR=0.0.0.0:9100
ADMIN_ADDR=0.0.0.0:9001

ADVERTISE_ADDR=node-01:9100

DATA_DIR=/data
REGION=us-east-1
```

Validate configuration at startup.

---

# 72. DOCKER COMPOSE — STANDALONE

Provide:

```text
docker-compose.standalone.yml
```

that starts one node with persistent storage.

---

# 73. DOCKER COMPOSE — CLUSTER

Provide:

```text
docker-compose.cluster.yml
```

with enough nodes to demonstrate actual failure tolerance.

Prefer at least:

```text
4 storage nodes
```

for development demonstration if the selected erasure policy requires it.

Each node must have independent persistent volumes.

Do NOT mount one shared host directory as the cluster's object storage.

---

# 74. CLUSTER DEMO

This should work:

```bash
docker compose -f docker-compose.cluster.yml up -d
```

Then:

```bash
aws \
  --endpoint-url http://localhost:9000 \
  s3 mb s3://photos
```

Upload:

```bash
aws \
  --endpoint-url http://localhost:9000 \
  s3 cp ./large-video.mp4 \
  s3://photos/videos/large-video.mp4
```

Download and verify checksum.

Then intentionally stop a storage node.

Example:

```bash
docker stop node-3
```

The object must remain readable if the configured durability scheme permits that failure.

Start the node again.

Healing should detect and repair missing/outdated shards where required.

---

# 75. AWS CLI COMPATIBILITY

Test commands such as:

```bash
aws --endpoint-url http://localhost:9000 s3 mb s3://test

aws --endpoint-url http://localhost:9000 \
    s3 cp file.bin s3://test/file.bin

aws --endpoint-url http://localhost:9000 \
    s3 ls s3://test/

aws --endpoint-url http://localhost:9000 \
    s3 cp s3://test/file.bin ./download.bin

aws --endpoint-url http://localhost:9000 \
    s3 rm s3://test/file.bin
```

Also test against an AWS SDK using endpoint override/path-style addressing as appropriate.

---

# 76. TESTING PYRAMID

Create:

```text
unit tests
component tests
integration tests
cluster tests
failure tests
compatibility tests
property tests where useful
benchmarks
```

---

# 77. DISTRIBUTED FAILURE TESTS

Automate scenarios such as:

```text
kill coordinator during PUT

kill storage node during PUT

kill storage node during GET

lose one disk

corrupt one shard

restart metadata leader

partition metadata leader

minority network partition

duplicate RPC

delayed RPC

client retry after timeout

node rejoins after long outage

disk returns with stale shards

delete during healing

overwrite during healing

node drain during uploads

cluster restart
```

Verify invariants after every test.

---

# 78. CRITICAL INVARIANTS

Encode these explicitly in tests.

Invariant 1:

```text
A committed object must never reference insufficient durable data.
```

Invariant 2:

```text
An uncommitted object must never become visible.
```

Invariant 3:

```text
A stale manifest must never replace a newer committed generation.
```

Invariant 4:

```text
GC must never delete data referenced by a live manifest.
```

Invariant 5:

```text
Healing must preserve object contents exactly.
```

Invariant 6:

```text
Minority partitions must not create conflicting metadata commits.
```

Invariant 7:

```text
Object bytes returned to a client must pass integrity verification.
```

---

# 79. PROPERTY TESTING

Use property-based testing where valuable for:

```text
range parser
SigV4 canonicalization
erasure reconstruction
placement
pagination
manifest serialization
state-machine transitions
```

For erasure coding:

Generate random data.

Encode it.

Randomly remove up to the supported number of shards.

Reconstruct.

Assert:

```text
original == reconstructed
```

---

# 80. PERFORMANCE TESTING

Benchmark:

```text
PUT throughput
GET throughput
small-object operations/sec
large-object throughput
range GET
multipart throughput
erasure encoding CPU
erasure decoding CPU
healing throughput
metadata operations
```

Test:

```text
1 KiB
64 KiB
1 MiB
100 MiB
1 GiB+
```

Do not put huge generated files into Git.

---

# 81. MEMORY REQUIREMENT

Memory usage must not scale linearly with object size.

Uploading:

```text
100 GB
```

must not require:

```text
100 GB RAM
```

Use bounded stripe buffers.

Document approximate memory consumption:

```text
concurrency × stripe size × shard buffers
```

---

# 82. GRACEFUL SHUTDOWN

On SIGTERM:

```text
mark node draining/not-ready
        ↓
stop new requests
        ↓
finish/cancel active operations safely
        ↓
flush metadata/storage
        ↓
leave cluster safely when appropriate
        ↓
shutdown
```

Do not silently corrupt in-progress writes.

---

# 83. SECURITY

Protect against:

```text
path traversal
malformed HTTP
XML attacks
oversized headers
oversized metadata
request smuggling where applicable
credential leakage
signature timing attacks
unauthorized internal RPC
SQL/query injection if SQL exists
resource exhaustion
malicious multipart uploads
malicious continuation tokens
```

Avoid unsafe Rust unless unavoidable and heavily justified.

---

# 84. TLS

Support TLS for S3 endpoints.

Cluster RPC should support/require mTLS in production.

Document certificate provisioning and rotation strategy.

---

# 85. NO SINGLE POINT OF FAILURE IN CLUSTER MODE

Cluster mode must not depend on:

```text
one metadata node
one coordinator
one storage node
one disk
one PostgreSQL server
```

for continued operation within the configured fault-tolerance limits.

Individual requests may have coordinators.

The system itself must not have one permanent coordinator.

---

# 86. STANDALONE → CLUSTER MIGRATION

Design a supported path from:

```text
standalone
```

to:

```text
cluster
```

Do not require rewriting every object through the S3 API if avoidable.

At minimum document a safe migration tool/workflow.

Potential command:

```bash
admin cluster migrate-from-standalone ...
```

Implement later if necessary, but ensure formats do not make migration impossible.

---

# 87. ON-DISK FORMAT VERSIONING

Persist:

```text
format_version
```

for:

```text
volume metadata
shard metadata
object manifests
cluster metadata
```

Do not assume the first on-disk representation will last forever.

Provide migration strategy.

---

# 88. PROTOCOL VERSIONING

Internal RPC must include protocol/version negotiation.

A rolling upgrade should eventually allow:

```text
v1 node
v2 node
```

to coexist during a controlled upgrade when compatible.

---

# 89. ROLLING UPGRADES

Design for:

```text
drain node
upgrade binary
restart
rejoin
heal/check
next node
```

Do not require full cluster shutdown for ordinary upgrades.

---

# 90. DEVELOPMENT PHASES

Implement incrementally.

## Phase 0 — Architecture

Before coding, produce:

```text
architecture
failure model
consistency model
metadata design
object manifest
erasure model
placement strategy
write protocol
read protocol
delete protocol
healing protocol
```

Identify invariants.

Do not start implementation until these fit together coherently.

---

## Phase 1 — Foundation

Implement:

```text
Cargo workspace
configuration
errors
logging
metrics skeleton
request IDs
node identity
volume identity
local storage engine
```

Add tests.

---

## Phase 2 — Metadata State Machine

Implement:

```text
bucket metadata
object metadata
manifests
generations
tombstones
multipart metadata
```

Initially exercise the state machine locally.

Add deterministic tests.

---

## Phase 3 — Standalone S3

Implement:

```text
CreateBucket
ListBuckets
HeadBucket
DeleteBucket

PutObject
GetObject
HeadObject
DeleteObject

ListObjectsV2
```

Use the same domain services that cluster mode will later call.

---

## Phase 4 — SigV4

Implement:

```text
SigV4 headers
presigned URLs
credential provider
authorization
```

Verify with AWS CLI.

---

## Phase 5 — Local Erasure Coding

Implement:

```text
streaming stripe encoder
shard storage
reconstruction
checksums
```

Test across multiple local volumes.

---

## Phase 6 — Internal RPC

Implement:

```text
node identity
authenticated RPC
PutShard
GetShard
DeleteShard
Health
```

Test two independent processes.

---

## Phase 7 — Cluster Membership

Implement:

```text
cluster ID
bootstrap
join
node registry
heartbeats
failure detection
```

---

## Phase 8 — Consensus Metadata

Implement Raft-backed metadata replication.

Test:

```text
leader failure
restart
minority partition
snapshot recovery
```

---

## Phase 9 — Distributed PUT/GET

Implement:

```text
placement
remote shard writes
write commit protocol
distributed GET
degraded GET
```

This is the first true distributed object-storage milestone.

---

## Phase 10 — Multipart

Implement distributed multipart upload.

---

## Phase 11 — Versioning

Implement distributed-safe versioning and delete markers.

---

## Phase 12 — Healing

Implement:

```text
missing shard repair
corruption repair
rejoin repair
```

---

## Phase 13 — Rebalancing

Implement:

```text
add node
drain node
add volume
remove volume
```

---

## Phase 14 — Scrubbing + GC

Implement:

```text
bit-rot scanning
temporary cleanup
orphan cleanup
tombstone cleanup
```

---

## Phase 15 — Production Hardening

Perform:

```text
fault injection
network partition tests
security review
load testing
long-running soak tests
resource exhaustion tests
rolling restart tests
```

---

# 91. CODING RULES

Use idiomatic Rust.

Prefer:

```rust
Result<T, Error>
```

over panics.

Avoid production-path:

```rust
unwrap()
expect()
```

unless the invariant is genuinely impossible to violate and documented.

Use:

```text
newtypes
enums
traits at infrastructure boundaries
structured errors
explicit state transitions
```

Avoid:

```text
god objects
global mutable state
global locks
unbounded channels
unbounded retries
```

---

# 92. ASYNC RULES

Never perform blocking disk or CPU-heavy work accidentally on Tokio core executor threads.

Identify:

```text
filesystem operations
fsync
erasure encoding
checksum calculation
compression if introduced
```

and use appropriate async/blocking/worker strategies.

Benchmark before making unnecessary complexity.

---

# 93. DOCUMENTATION

Create:

```text
README.md

docs/
  architecture.md
  consistency.md
  durability.md
  erasure-coding.md
  metadata.md
  cluster.md
  failure-recovery.md
  security.md
  compatibility.md
  operations.md
```

Use Mermaid diagrams.

Document exactly which S3 APIs are:

```text
supported
partially supported
unsupported
```

Never advertise complete AWS S3 compatibility unless actually verified.

---

# 94. ARCHITECTURE DOCUMENTATION

`architecture.md` must explain at least:

```text
standalone architecture
cluster architecture
PUT flow
GET flow
DELETE flow
multipart flow
metadata consensus
placement
erasure coding
healing
rebalancing
GC
```

---

# 95. OPERATIONS DOCUMENTATION

Explain:

```text
create cluster
join node
add disk
replace disk
drain node
remove node
restart cluster
upgrade cluster
recover failed node
inspect healing
inspect capacity
rotate credentials
rotate certificates
```

---

# 96. DEFINITION OF DONE — STANDALONE

This must work:

```bash
server --mode standalone
```

Then:

```bash
aws --endpoint-url http://localhost:9000 \
    s3 mb s3://test
```

Upload a multi-gigabyte object.

Memory must remain bounded.

Download it.

Verify checksum.

Restart the server.

Download it again successfully.

---

# 97. DEFINITION OF DONE — CLUSTER

Start multiple independent nodes.

For example:

```text
node-01
node-02
node-03
node-04
node-05
node-06
```

Each has independent storage.

Upload a large object.

Verify its physical shards are distributed according to placement policy.

Download it and verify checksum.

Stop one node.

Download again.

If the durability policy allows that failure, download must succeed.

Corrupt a shard.

Download must return correct data using healthy shards.

Healing must eventually replace the corrupted shard.

Restart the failed node.

Cluster must converge back to healthy state.

---

# 98. DEFINITION OF DONE — METADATA FAILURE

In a multi-node metadata consensus group:

```text
stop current leader
```

A new leader must be elected when quorum remains.

After recovery, S3 writes must resume without conflicting metadata histories.

Test minority partitions.

Minority nodes must not accept conflicting writes.

---

# 99. DEFINITION OF DONE — DATA INTEGRITY

For every integration test involving object storage:

```text
SHA256(uploaded bytes)
==
SHA256(downloaded bytes)
```

Test this after:

```text
normal operation
node failure
node restart
shard corruption
healing
rebalance
multipart upload
server restart
```

Data correctness is more important than benchmark numbers.

---

# 100. NON-GOALS FOR INITIAL RELEASE

Do not let these delay the core storage engine:

```text
web management dashboard
full AWS IAM clone
S3 Glacier
cross-region replication
S3 Select
Lambda/event ecosystem
every AWS S3 API
multi-region consensus
complex billing
```

Design extensibility where appropriate but do not implement speculative features.

---

# 101. IMPORTANT DESIGN PRINCIPLE

Do NOT build:

```text
Single Node S3
      ↓
finish it
      ↓
later somehow make it distributed
```

Build:

```text
                Common S3 Layer
                      │
                      ▼
                Object Service
                      │
                      ▼
              Durability Layer
                /           \
               /             \
      Standalone             Cluster
          │                     │
     local shards         distributed shards
```

Standalone is simply the smallest deployment topology of the same architecture.

---

# 102. MOST IMPORTANT DISTRIBUTED WRITE RULE

Never make an object visible before its configured durability requirements have been satisfied.

The general protocol should resemble:

```text
Client
  │
  │ PUT
  ▼
Coordinator
  │
  ├── calculate placement
  │
  ├── stream encoder
  │
  ├──────────────► Node A
  │──────────────► Node B
  │──────────────► Node C
  │──────────────► Node D
  │
  │ wait for required durable acknowledgements
  │
  ▼
Commit Object Manifest
through metadata consensus
  │
  ▼
Object becomes visible
  │
  ▼
200 OK
```

Failure before manifest commit:

```text
object is NOT visible
```

and written shards become eventual GC candidates.

---

# 103. MOST IMPORTANT READ RULE

Never return bytes known to have failed integrity validation.

When a shard fails checksum verification:

```text
reject shard
    ↓
use another healthy shard
    ↓
reconstruct
    ↓
serve verified data
    ↓
schedule repair
```

---

# 104. MOST IMPORTANT DELETE RULE

Logical deletion precedes physical deletion.

```text
metadata tombstone
      ↓
object becomes inaccessible
      ↓
grace period
      ↓
physical GC
```

This must remain safe across retries, crashes, healing, and rebalancing.

---

# 105. MOST IMPORTANT CONSENSUS RULE

Do not invent a simplified distributed consensus protocol.

Use a proven algorithm/library.

Object data does NOT belong in the consensus log.

Consensus protects authoritative metadata and cluster state.

---

# 106. MOST IMPORTANT ENGINEERING RULE

At the end of every phase:

```bash
cargo fmt --check

cargo clippy \
    --workspace \
    --all-targets \
    --all-features \
    -- -D warnings

cargo test --workspace
```

Fix failures before continuing.

Where applicable also run:

```bash
cargo audit
```

Do not suppress warnings simply to obtain a green build.

---

# 107. IMPLEMENTATION OUTPUT FORMAT

Do not dump thousands of lines of disconnected code immediately.

For each phase:

1. State the goal.
2. Explain important architectural decisions.
3. Show the directory changes.
4. Implement complete files.
5. Add tests.
6. Compile.
7. Run tests.
8. Fix failures.
9. Run Clippy.
10. Update documentation.
11. State remaining limitations.

Never use pseudo-implementations such as:

```rust
todo!()
```

for functionality claimed as complete.

Do not mock core storage behavior in production code merely to progress to the next phase.

---

# 108. FIRST RESPONSE

Before generating implementation code, produce the complete technical design.

The first response must contain:

1. System architecture.
2. Standalone architecture.
3. Cluster architecture.
4. Cargo workspace structure.
5. Major Rust traits/interfaces.
6. Metadata architecture.
7. Raft architecture.
8. Object manifest design.
9. On-disk shard format.
10. Erasure coding strategy.
11. Small-object strategy.
12. Placement algorithm.
13. Write quorum semantics.
14. Detailed distributed PutObject sequence.
15. Detailed distributed GetObject sequence.
16. Detailed DeleteObject sequence.
17. Multipart architecture.
18. Versioning architecture.
19. Cluster bootstrap/join procedure.
20. Failure-detection strategy.
21. Healing algorithm.
22. Rebalancing algorithm.
23. Garbage-collection strategy.
24. Crash-consistency analysis.
25. Network-partition behavior.
26. Security architecture.
27. Testing/fault-injection strategy.
28. Implementation phases.
29. Key technical risks and mitigations.
30. Explicit system invariants.

For every distributed protocol, analyze what happens if the coordinator crashes after every important step.

Do not start Phase 1 until the architecture has been made internally consistent.

---

# FINAL OBJECTIVE

The finished project must be one Rust object-storage platform capable of operating as:

```text
Developer laptop
       ↓
single process
single disk
```

or:

```text
Standalone production server
       ↓
single node
multiple disks
optional local erasure coding
```

or:

```text
Production cluster
       ↓
multiple Rust nodes
multiple disks per node
distributed metadata
erasure coding
quorum-based durability
automatic healing
rebalancing
failure recovery
```

while exposing the same S3-compatible interface:

```text
                    AWS CLI
                       │
                    AWS SDK
                       │
                  S3 Clients
                       │
                       ▼
              ┌─────────────────┐
              │ S3-Compatible   │
              │      API        │
              └────────┬────────┘
                       │
                 Object Service
                       │
                 Durability Layer
                    /       \
                   /         \
                  ▼           ▼
            Standalone     Cluster
                │             │
                ▼             ▼
             Shards       Distributed
                           Shards
                              │
                    ┌─────────┼─────────┐
                    ▼         ▼         ▼
                  Node A    Node B    Node C
```

The final system should feel like **one storage engine that scales from one machine to a real fault-tolerant cluster**, not two unrelated implementations sharing an HTTP API.

Begin with the architecture and distributed correctness model. Do not begin by writing the Axum routes.
