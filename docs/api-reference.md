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

`GetObject` and `HeadObject` support a single byte range (`Range: bytes=0-99`, `bytes=100-`
or `bytes=-100`), and `If-Match` and `If-Range`:

- A satisfied range returns `206 Partial Content` with `Content-Range`, and reads only the
  stripes it falls in.
- A range starting past the end, or any range on an empty object, returns
  `416 InvalidRange`.
- Several ranges at once, or a malformed `Range`, return the whole object with `200`.
- `If-Match` returns `412 PreconditionFailed` if the object has changed. `If-Range` returns
  the whole object if it has changed.

All routes are path-style (`http://host:port/{bucket}/{key}`); there is no
virtual-hosted-style (`{bucket}.host`) routing.

## Not yet implemented

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
The admin API creates additional credentials — see [admin.md](admin.md).

Presigned URLs must use SigV4. boto3 can produce older SigV2 URLs by default, which get a
`403`, so set `signature_version="s3v4"` when generating them.

## Upload bodies

`x-amz-content-sha256` on `PutObject` and `UploadPart` says how the body is sent:

| Value | Body | Checked |
|---|---|---|
| A SHA-256 in hex | Plain | The body must match the hash |
| `UNSIGNED-PAYLOAD`, or no header | Plain | Nothing |
| `STREAMING-UNSIGNED-PAYLOAD-TRAILER` | `aws-chunked` | The trailer's checksum, if one is declared |
| `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` | `aws-chunked` | Every chunk signature |
| `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER` | `aws-chunked` | Every chunk signature, the trailer's signature and its checksum |

The `aws-chunked` framing is removed before storage, so the object holds only its real
bytes. Any other value is refused with `501 NotImplemented` rather than stored as-is, which
includes the SigV4A `STREAMING-AWS4-ECDSA-*` modes.

A checksum can also be sent as an `x-amz-checksum-*` header or an `x-amz-trailer`, using
CRC32, CRC32C, CRC64NVME, SHA-1 or SHA-256. Checksums aren't stored, and aren't returned on
responses.

A failed check fails the upload, and any existing object at that key is left unchanged.
Shards already written by the failed upload aren't cleaned up yet, the same as for a client
that disconnects partway through.

## Console API

Behind the web console, and only served when
[sign-in is configured](configuration.md#sign-in-with-loony). It uses JSON, including for
errors: `{"code": "...", "message": "..."}`. Only the origins of the configured redirect
URIs can call it from a browser.

| Operation | Request | Notes |
|---|---|---|
| Sign in | `POST /_console/v1/session` | Unsigned. Takes `code`, `code_verifier`, `redirect_uri` and `nonce`, and returns `access_key`, `secret_key`, `expires_at` and `user`. Limited to 30 attempts a minute per client. |
| Who am I | `GET /_console/v1/me` | |
| Sign out | `DELETE /_console/v1/session` | The key stops working at once, along with the project keys opened from it. |
| List projects | `GET /_console/v1/projects` | Returns `created`, `joined` and `invites`. |
| Create a project | `POST /_console/v1/projects` | `{"name"}`. Anyone signed in can, up to 10 each. It starts unpaid and can't store anything until it's subscribed. |
| View a project | `GET /_console/v1/projects/{id}` | |
| Rename a project | `PATCH /_console/v1/projects/{id}` | `{"name"}`. Owner only. |
| Delete a project | `DELETE /_console/v1/projects/{id}` | Owner only. Refused while it has buckets. Removes its keys, members and invites. |
| Open a project | `POST /_console/v1/projects/{id}/session` | Returns a temporary key for the LS3 API, which ends with the sign-in. Opening it again returns the same key. |
| Leave a project | `POST /_console/v1/projects/{id}/leave` | |
| List access keys | `GET /_console/v1/projects/{id}/keys` | Without secrets. |
| Create an access key | `POST /_console/v1/projects/{id}/keys` | `{"name"}`. Only this response has the secret. A project can have 100. |
| Change an access key | `PATCH /_console/v1/projects/{id}/keys/{key}` | `{"enabled"}`. |
| Delete an access key | `DELETE /_console/v1/projects/{id}/keys/{key}` | |
| List members | `GET /_console/v1/projects/{id}/members` | |
| Change a member's role | `PATCH /_console/v1/projects/{id}/members/{user}` | `{"role": "owner" or "member"}`. Owner only. |
| Remove a member | `DELETE /_console/v1/projects/{id}/members/{user}` | Owner only. Ends their session keys for this project. |
| List invites | `GET /_console/v1/projects/{id}/invites` | Owner only. |
| Invite someone | `POST /_console/v1/projects/{id}/invites` | `{"email", "role"}`. Owner only. Expires after 7 days. No email is sent: it shows up when they sign in. |
| Cancel an invite | `DELETE /_console/v1/projects/{id}/invites/{invite}` | Owner only. |
| Accept an invite | `POST /_console/v1/invites/{invite}/accept` | Only for the verified email it was sent to. |
| Decline an invite | `POST /_console/v1/invites/{invite}/decline` | Only for the verified email it was sent to. |
| Billing overview | `GET /_console/v1/billing` | Returns `balance_paise`, the `plans`, and `checkout`: the payment provider's name, or `null` when paying isn't available. |
| Credit history | `GET /_console/v1/billing/ledger` | Newest first. Each entry has the change, the balance after it, and what it was for. |
| Redeem a gift code | `POST /_console/v1/billing/gift-codes/redeem` | `{"code"}`. Case, spaces and dashes are ignored. Each person can redeem a code once. Limited to 10 attempts a minute per person and per client address. |
| Start a checkout | `POST /_console/v1/billing/checkouts` | `{"amount_paise", "project_id"?, "plan_id"?}`, from ₹1 to ₹1,00,000. With a project, a successful payment also subscribes it. Only when `LS3_BILLING_MODE=test`; otherwise `checkout_unavailable`. |
| View a checkout | `GET /_console/v1/billing/checkouts/{id}` | Only your own. |
| Finish a test checkout | `POST /_console/v1/billing/checkouts/{id}/confirm` | `{"outcome": "succeed" or "fail"}`. Completing a checkout twice doesn't add credit twice. |
| Payment webhook | `POST /_console/v1/billing/webhooks/{provider}` | Unsigned: the provider verifies it. Nothing real uses it yet. |
| View a subscription | `GET /_console/v1/projects/{id}/subscription` | Any member. Returns its `state` (`unpaid`, `active`, `grace`, `read_only` or `suspended`), plan, period, payer, and `usage` with `bytes`, `objects`, `pending_bytes` and `limit_bytes`. |
| Subscribe | `POST /_console/v1/projects/{id}/subscription` | `{"plan_id"?}`. Owner only. Pays the first period from your credit. |
| Renew now | `POST /_console/v1/projects/{id}/subscription/renew` | Owner only. For a lapsed project: pays from your credit and makes it active again. A project in grace continues from its old period end. |
| Cancel | `POST /_console/v1/projects/{id}/subscription/cancel` | Owner only. It keeps working until the period ends, then becomes read-only. Cancelling during grace makes it read-only at once. |
| Undo a cancel | `POST /_console/v1/projects/{id}/subscription/resume` | Owner only, before the period ends. |

Everything except signing in is signed with the session key.

A project owns its buckets, and its keys only reach those buckets. Access keys belong to
the project, so they keep working when the member who created them leaves or is disabled.
If that person could have kept a copy, replace the key. Disabling a project stops all its
keys while it's off.

Signed requests use SigV4 in the `Authorization` header with the real body hash. A
presigned URL isn't accepted.

A sign-in's session key is only for this API: the LS3 API refuses it with `AccessDenied`.
Other keys are refused here with `wrong_key`.

| Code | Status | Meaning |
|---|---|---|
| `invalid_request` | 400 | The request is malformed, or the return address isn't configured |
| `sign_in_failed` | 401 | The identity provider or its token was refused |
| `unauthorized` | 401 | The signature is missing or wrong, or the session has ended |
| `session_expired` | 401 | The session key has expired, so sign in again |
| `clock_skew` | 401 | The client's clock is more than 15 minutes off |
| `account_disabled` | 403 | An admin has disabled this user |
| `wrong_key` | 403 | This key isn't a session key |
| `not_found` | 404 | It doesn't exist, or you can't see it. These look identical on purpose |
| `owner_only` | 403 | Only a project Owner can do this |
| `subscription_required` | 403 | The project isn't paid for yet |
| `project_read_only` | 403 | The subscription lapsed or was cancelled, so nothing can be added, changed or deleted |
| `project_suspended` | 403 | Read-only for 30 days and still unpaid, so its keys stop working |
| `quota_exceeded` | 403 | The project has used all the storage its plan allows |
| `insufficient_credit` | 402 | Not enough credit to pay for this |
| `checkout_unavailable` | 404 | Paying isn't turned on for this server |
| `gift_code_invalid` | 404 | No such gift code, or it's disabled |
| `gift_code_expired` | 410 | |
| `already_paid`, `no_subscription` | 409 | |
| `gift_code_used_up`, `gift_code_already_redeemed` | 409 | |
| `project_disabled` | 403 | An admin has disabled this project |
| `email_not_verified` | 403 | Accepting an invite needs a verified email address |
| `project_not_empty` | 409 | Delete the project's buckets first |
| `last_owner` | 409 | A project must keep at least one Owner |
| `project_limit_reached`, `key_limit_reached` | 409 | Over the limit |
| `invite_exists`, `already_member`, `invite_expired` | 409 | |
| `too_many_requests` | 429 | Too many sign-in or gift code attempts |
| `identity_provider_unavailable` | 502 | The identity provider can't be reached |
| `unavailable` | 503 | This node can't reach the metadata leader |

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
| `InvalidRange` | 416 | |
| `PreconditionFailed` | 412 | `If-Match` didn't match the object |
| `ExpiredToken` | 400 | This key has passed its expiry |
| `BadDigest` | 400 | The body doesn't match its declared hash or checksum |
| `IncompleteBody` | 400 | An `aws-chunked` body is malformed, cut short, or the wrong length |
| `NotImplemented` | 501 | An upload body mode this server doesn't support |
| `InvalidArgument` | 400 | |
| `NoSuchUpload` | 404 | Unknown, aborted, already-completed, or wrong-bucket/key `uploadId` |
| `InvalidPart` | 400 | `CompleteMultipartUpload` referenced a part number never recorded, or gave an ETag that doesn't match what was recorded |
| `InvalidPartOrder` | 400 | `CompleteMultipartUpload`'s part list wasn't strictly ascending by part number |
| `AccessDenied` | 403 | Missing signature, or a credential touching a bucket it doesn't own |
| `SignatureDoesNotMatch` | 403 | Signature verification failed |
| `InvalidAccessKeyId` | 403 | Access key doesn't correspond to a known credential |
| `RequestTimeTooSkewed` | 403 | Request timestamp too far from server time |
| `QuotaExceeded` | 403 | The upload would take the project over its plan's storage. Checked when an upload starts if its size is declared, and again when it's stored, counting uploads still in progress |
| `ProjectReadOnly` | 403 | The project isn't paid for, or its subscription lapsed or was cancelled. Reads still work until it's suspended |
| `InternalError` | 500 | Anything internal (storage/metadata failure) — details go to the server's log, never the response body |

## Response headers

`GetObject`/`HeadObject` responses include `ETag` (content hash — MD5-based for a
single-part object, `hex(MD5(concat(part MD5s)))-N` for an object completed via
multipart, the upstream protocol's own documented convention) and `Last-Modified` in the format standard
clients' transfer managers expect. Every response also carries `x-amz-request-id` (a
UUIDv7), useful for correlating a client-side error against the server's logs.
