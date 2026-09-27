# LS3 API reference

What `loony-ls3` actually implements: routes, auth, validation rules, durability
behavior, and error codes. This describes *this server's* behavior precisely. The LS3
API is wire-compatible with S3; for the general protocol itself, see the upstream S3 API
reference.

## Supported operations

| Operation | Method + path | Notes |
|---|---|---|
| ListBuckets | `GET /` | Only buckets owned by the authenticated credential. |
| CreateBucket | `PUT /{bucket}` | |
| DeleteBucket | `DELETE /{bucket}` | Fails with `BucketNotEmpty` unless the bucket has no objects. |
| HeadBucket | `HEAD /{bucket}` | |
| ListObjectsV2 | `GET /{bucket}?list-type=2` | `prefix`, `delimiter`, `max-keys` (default/max 1000), `continuation-token`, `start-after` all supported. |
| PutObject | `PUT /{bucket}/{key}` | Single request body only. For anything large, prefer multipart (below) — the web UI's uploader still sends one request per object, no chunking. |
| GetObject | `GET /{bucket}/{key}` | |
| HeadObject | `HEAD /{bucket}/{key}` | |
| DeleteObject | `DELETE /{bucket}/{key}` | Idempotent: deleting an already-absent key is not an error. |
| CreateMultipartUpload | `POST /{bucket}/{key}?uploads` | Returns an `UploadId`; any API node can continue the upload afterward (multipart state is in the Raft-replicated metadata store, not pinned to whichever node handled Create). |
| UploadPart | `PUT /{bucket}/{key}?partNumber=N&uploadId=X` | `N` is 1-10000. Re-uploading the same `partNumber` before Complete overwrites what was recorded. |
| ListParts | `GET /{bucket}/{key}?uploadId=X` | Every part recorded so far, regardless of whether it'll end up in the eventual Complete. |
| CompleteMultipartUpload | `POST /{bucket}/{key}?uploadId=X` | Body: `<CompleteMultipartUpload><Part><PartNumber>N</PartNumber><ETag>"..."</ETag></Part>...</CompleteMultipartUpload>`, parts in ascending order. Validated against recorded parts and committed atomically — same visibility mechanism as a plain PUT. |
| AbortMultipartUpload | `DELETE /{bucket}/{key}?uploadId=X` | Idempotent. Already-written part shards become orphan-GC candidates (not yet implemented) rather than being deleted synchronously. |

All routes are path-style (`http://host:port/{bucket}/{key}`); there is no
virtual-hosted-style (`{bucket}.host`) routing.

## Not yet implemented

- **Range requests** — the `Range` header on `GetObject` is silently ignored; the full
  object is always returned rather than an error or a `206 Partial Content`. This means
  any client that downloads large objects with parallel ranged GETs (many CLIs do,
  above some size threshold, independent of multipart) will produce a corrupted local
  file — each parallel request gets the *full* object instead of its slice. Disable
  parallel downloads until Range support lands (e.g. rclone's
  `--multi-thread-streams 0`), or use a single plain GET.
- **`x-amz-meta-*` response headers** — user metadata given to `PutObject`/
  `CreateMultipartUpload` is stored and preserved correctly (round-trips through a
  multipart Complete too), but `GetObject`/`HeadObject` never echo it back as response
  headers yet.
- **Bucket versioning** — the domain model has a `versioning_state` field, but it's
  always `Disabled`; only the current version of an object is ever retained.
- **Object ACLs, bucket policies, lifecycle rules, CORS-per-bucket configuration,
  tagging, replication.**
- **Anonymous/public access** — every request needs a valid SigV4 signature, full stop.

## Authentication

SigV4 only, both forms the upstream protocol supports:

- **Header-based**: an `Authorization: <signing algorithm> Credential=...` header plus
  `x-amz-date` and `x-amz-content-sha256`.
- **Presigned query string**: `X-Amz-Algorithm`/`X-Amz-Credential`/`X-Amz-Signature`
  etc. as query parameters — no `Authorization` header needed by the party using the
  link. Presigned URLs are generated entirely client-side (`rclone link`, an SDK's
  presigner, or the web UI's "copy link" action) — there's no server endpoint
  that mints one for you.

Every bucket and object is owned by the credential that created it (ownership-based
authorization, not ACLs) — a different credential gets `AccessDenied`/`NoSuchBucket`
rather than seeing another owner's data, even if it can prove it knows the bucket name.

Credentials are managed as records in the metadata store, keyed by access key. The one
guaranteed to exist is whatever `LS3_ROOT_ACCESS_KEY`/`LS3_ROOT_SECRET_KEY` (or the
dev-derived fallback) resolves to at startup — see [configuration.md](configuration.md).
There's no API to create additional credentials yet.

## Naming rules

**Bucket names** — enforced exactly, same as the upstream protocol:
- 3–63 characters
- lowercase letters, digits, dots (`.`), and hyphens (`-`) only
- must start and end with a lowercase letter or digit
- no consecutive dots, no dot adjacent to a hyphen
- must not be formatted like an IPv4 address (e.g. `192.168.1.1`)

**Object keys**:
- 1–1024 bytes
- no null bytes, no control characters
- otherwise arbitrary — including characters like `/` (used for the prefix/delimiter
  listing semantics above, but not treated as filesystem path separators; there is no
  path-traversal special-casing because keys never touch the filesystem directly, see
  `architecture.md` §51)

## Durability behavior

Every `PutObject` picks a durability strategy automatically based on size — this isn't
configurable per-request yet:

- **Objects under 512 KiB**: replicated (whole-object copies), not erasure-coded — the
  fixed overhead of erasure coding isn't worth it at that size.
- **Objects 512 KiB and larger**: erasure-coded, streamed in stripes. The exact
  `(data shards, parity shards)` split depends on how many local volumes
  (`LS3_VOLUME_PATHS`) the node has, so a small setup degrades gracefully instead of
  co-locating multiple shards of one stripe on the same volume:

  | Volumes available | Scheme (data+parity) |
  |---|---|
  | 0–1 | 1+0 (no redundancy — single volume, nothing to spread across) |
  | 2 | 1+1 |
  | 3 | 2+1 |
  | 4 | 2+2 |
  | 5 | 3+2 |
  | 6+ | 4+2 (the architecture doc's documented default) |

Every shard's checksum is verified on read; a `GetObject` against an object with a
missing or corrupted shard reconstructs it from the remaining shards/parity rather than
failing, as long as enough shards survive for the chosen scheme.

## Error responses

Errors are XML `<Error>` bodies matching the upstream protocol's shape and vocabulary:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchBucket</Code>
  <Message>The specified bucket does not exist</Message>
  <RequestId>...</RequestId>
  <Resource>/my-bucket</Resource>
</Error>
```

| Code | HTTP status | When |
|---|---|---|
| `NoSuchBucket` | 404 | |
| `NoSuchKey` | 404 | |
| `BucketAlreadyExists` | 409 | |
| `BucketNotEmpty` | 409 | `DeleteBucket` on a non-empty bucket |
| `InvalidBucketName` | 400 | |
| `InvalidArgument` | 400 | |
| `NoSuchUpload` | 404 | Unknown, aborted, already-completed, or wrong-bucket/key `uploadId` |
| `InvalidPart` | 400 | `CompleteMultipartUpload` referenced a part number never recorded, or gave an ETag that doesn't match what was recorded |
| `InvalidPartOrder` | 400 | `CompleteMultipartUpload`'s part list wasn't strictly ascending by part number |
| `AccessDenied` | 403 | Missing signature, or a credential touching a bucket it doesn't own |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |
| `InvalidAccessKeyId` | 403 | Access key doesn't correspond to a known credential |
| `RequestTimeTooSkewed` | 403 | Request timestamp too far from server time |
| `InternalError` | 500 | Anything internal (storage/metadata failure) — details go to the server's log, never the response body |

## Response headers

`GetObject`/`HeadObject` responses include `ETag` (content hash — MD5-based for a
single-part object, `hex(MD5(concat(part MD5s)))-N` for an object completed via
multipart, the upstream protocol's own documented convention) and `Last-Modified` in the format standard
clients' transfer managers expect. Every response also carries `x-amz-request-id` (a
UUIDv7), useful for correlating a client-side error against the server's logs.
