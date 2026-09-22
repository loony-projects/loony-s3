# Usage

How to build, run, and talk to `loony-s3` — as a single-process server, from the
AWS CLI or any S3 SDK, and from the bundled web UI. For *why* it's built this way, see
[`architecture.md`](architecture.md). For every environment variable and CLI flag, see
[`configuration.md`](configuration.md). For the exact S3 API surface (which operations
exist, naming rules, error codes), see [`api-reference.md`](api-reference.md). For
running more than one node, see [`cluster.md`](cluster.md).

## Build

```bash
cargo build --release
# binary at target/release/loony-server
```

A debug build (`cargo build`, binary at `target/debug/loony-server`) is fine for local use
and is what the examples below use.

## Run standalone

Two environment variables are required; everything else has a sane default.

```bash
S3_MODE=standalone \
S3_DATA_DIR=/tmp/loony-dev \
cargo run --bin loony-server
```

On first start, the server:

- mints a persistent node identity and writes it to `$S3_DATA_DIR/NODE_ID`
- creates a local volume under `$S3_DATA_DIR/volumes/vol-0` (override with
  `S3_VOLUME_PATHS`, see [configuration](configuration.md))
- opens its metadata store at `$S3_DATA_DIR/meta.redb` (a real single-voter Raft group,
  not just a plain database — see `architecture.md` §5)
- seeds a root credential — from `S3_ROOT_ACCESS_KEY`/`S3_ROOT_SECRET_KEY` if you set
  them, otherwise a credential deterministically derived from the node's identity, which
  the server prints to its log loudly labeled as dev-only. Anything beyond local
  experimentation should set those two variables explicitly.
- starts serving the S3 API on `S3_BIND_ADDR` (default `0.0.0.0:9000`)

```
server: using a dev-default root credential (set S3_ROOT_ACCESS_KEY / S3_ROOT_SECRET_KEY
for anything beyond local testing):
  access key: AKIADEV...
  secret key: 6f2c9a...
server: S3 API listening on http://0.0.0.0:9000
```

Stop it with Ctrl-C; it drains in-flight requests before exiting. Restarting with the
same `S3_DATA_DIR` resumes exactly where it left off — buckets, objects, and credentials
all persist (verified across restarts as part of Phase 8's testing).

## Talk to it

The API is real S3: SigV4-signed, XML-bodied, path-style routing
(`http://host:port/{bucket}/{key}`). Any S3 client works — AWS CLI, `boto3`, the AWS SDK
for your language, `curl` with your own SigV4 signer, or the bundled web UI.

### AWS CLI

```bash
export AWS_ACCESS_KEY_ID=<access key from the server's startup log>
export AWS_SECRET_ACCESS_KEY=<secret key from the server's startup log>
export AWS_DEFAULT_REGION=us-east-1   # must match S3_REGION (default us-east-1)

ENDPOINT=http://127.0.0.1:9000

aws --endpoint-url $ENDPOINT s3 mb s3://my-bucket
aws --endpoint-url $ENDPOINT s3 cp ./photo.jpg s3://my-bucket/photo.jpg
aws --endpoint-url $ENDPOINT s3 ls s3://my-bucket/
aws --endpoint-url $ENDPOINT s3 cp s3://my-bucket/photo.jpg ./downloaded.jpg
aws --endpoint-url $ENDPOINT s3 rm s3://my-bucket/photo.jpg
aws --endpoint-url $ENDPOINT s3 rb s3://my-bucket
```

`aws s3 sync` works too, and is the fastest way to round-trip a whole directory for a
smoke test:

```bash
aws --endpoint-url $ENDPOINT s3 sync ./local-dir/ s3://my-bucket/prefix/
```

A presigned URL (no `Authorization` header needed by the requester) is generated the
same way as against real S3:

```bash
aws --endpoint-url $ENDPOINT s3 presign s3://my-bucket/photo.jpg --expires-in 300
```

### boto3 / any AWS SDK

Point the SDK's endpoint override at the server and use path-style addressing (the
server doesn't do virtual-hosted-style `bucket.host` routing):

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="http://127.0.0.1:9000",
    aws_access_key_id="...",
    aws_secret_access_key="...",
    region_name="us-east-1",
    config=boto3.session.Config(s3={"addressing_style": "path"}),
)

s3.create_bucket(Bucket="my-bucket")
s3.put_object(Bucket="my-bucket", Key="hello.txt", Body=b"hello")
print(s3.get_object(Bucket="my-bucket", Key="hello.txt")["Body"].read())
```

### curl (for scripting or debugging without an SDK)

Real S3 requests need a valid SigV4 signature — `curl` alone can't produce one. For
quick manual checks, a presigned URL (minted via the AWS CLI, above, or the web UI's
"copy link" action) is a plain signed `GET` any HTTP client can follow:

```bash
curl -o photo.jpg "$(aws --endpoint-url $ENDPOINT s3 presign s3://my-bucket/photo.jpg)"
```

## Run the web UI

The frontend (`frontend/`) is a Vite/React app that signs every request with SigV4
itself, directly from the browser — there's no backend-for-frontend or proxy in front of
the S3 API.

```bash
cd frontend
npm install
cp .env.example .env.local
# edit .env.local: VITE_API_URL must point at the running server's S3 API
#   (e.g. http://127.0.0.1:9000), VITE_REGION must match S3_REGION
npm run dev
# open http://localhost:5173
```

Sign in with the same access key / secret key pair the server printed at startup, the
server's URL, and its region. The credentials are kept client-side (in the browser) and
used to sign every request — nothing is sent to any third party.

From there: create a bucket, drag-and-drop files to upload, list/download/delete
objects, copy a presigned link for a file. The web UI's own uploader always sends a
single `PUT` regardless of size (no chunking) — the server itself does support
multipart upload (`aws s3 cp`/`aws s3api create-multipart-upload` and friends), see
[api-reference.md](api-reference.md).

Production build: `npm run build` (output in `frontend/dist/`, a static site — serve it
from any static host, pointed at a real `VITE_API_URL`).

**CORS**: the server sends permissive CORS headers (any origin) by default, so the
frontend works out of the box even though it's served from a different origin/port than
the API. See `crates/api/src/lib.rs`'s `CorsLayer` if you need to lock this down for a
real deployment.

## Multiple local volumes

To exercise erasure coding (objects ≥512 KiB get erasure-coded rather than replicated;
see [api-reference.md](api-reference.md) for the exact scheme table), give the node more
than one volume path:

```bash
S3_MODE=standalone \
S3_DATA_DIR=/tmp/loony-dev \
S3_VOLUME_PATHS=/tmp/loony-dev/vol-a,/tmp/loony-dev/vol-b,/tmp/loony-dev/vol-c \
cargo run --bin loony-server
```

## Running more than one node

See [cluster.md](cluster.md) — and read its "what this does and doesn't mean today"
section before relying on it for anything beyond bootstrap/join/heartbeat mechanics.

## Troubleshooting

- **`server: invalid configuration: ...`** — a required env var is missing or malformed;
  the message names which one. See [configuration.md](configuration.md).
- **`--node-id / S3_NODE_ID does not match this data directory's persisted NODE_ID`** —
  you pointed an explicit `--node-id`/`S3_NODE_ID` at a data directory that already has a
  different identity persisted in `NODE_ID`. Drop the override, or point at an empty
  data directory.
- **SigV4 `SignatureDoesNotMatch`** — usually a region or clock-skew mismatch. Confirm
  the client's region matches `S3_REGION` (default `us-east-1`) and the client's clock
  is close to the server's (requests more than ~15 minutes off are also rejected as
  `RequestTimeTooSkewed`).
- **`403` with no body on a bare `GET /`** — expected: every route requires a valid
  SigV4 signature, including the root path.
