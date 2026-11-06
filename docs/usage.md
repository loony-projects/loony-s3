# Usage

How to build, run, and talk to `loony-ls3` — as a single-process server, from any
S3-compatible client, and from the bundled web UI. For *why* it's built this way, see
[`architecture.md`](architecture.md). For every environment variable and CLI flag, see
[`configuration.md`](configuration.md). For the exact LS3 API surface (which operations
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
LS3_MODE=standalone \
LS3_DATA_DIR=/tmp/loony-dev \
cargo run --bin loony-server
```

On first start, the server:

- mints a persistent node identity and writes it to `$LS3_DATA_DIR/NODE_ID`
- creates a local volume under `$LS3_DATA_DIR/volumes/vol-0` (override with
  `LS3_VOLUME_PATHS`, see [configuration](configuration.md))
- opens its metadata store at `$LS3_DATA_DIR/meta.redb` (a real single-voter Raft group,
  not just a plain database — see `architecture.md` §5)
- seeds a root credential — from `LS3_ROOT_ACCESS_KEY`/`LS3_ROOT_SECRET_KEY` if you set
  them, otherwise a credential deterministically derived from the node's identity, which
  the server prints to its log loudly labeled as dev-only. Anything beyond local
  experimentation should set those two variables explicitly.
- starts serving the LS3 API on `LS3_BIND_ADDR` (default `0.0.0.0:7353`)

```
server: using a dev-default root credential (set LS3_ROOT_ACCESS_KEY / LS3_ROOT_SECRET_KEY
for anything beyond local testing):
  access key: AKIADEV...
  secret key: 6f2c9a...
server: LS3 API listening on http://0.0.0.0:7353
```

Stop it with Ctrl-C; it drains in-flight requests before exiting. Restarting with the
same `LS3_DATA_DIR` resumes exactly where it left off — buckets, objects, and credentials
all persist (verified across restarts as part of Phase 8's testing).

## Talk to it

The API is S3-compatible: SigV4-signed, XML-bodied, path-style routing
(`http://host:port/{bucket}/{key}`). Any S3-compatible client works; the examples below
use the bundled scripts and [rclone](https://rclone.org).

### Bundled scripts

The scripts read their credentials from `scripts/.env` (gitignored) and refuse to start
if one is missing; variables already set in your shell take precedence over the file.

```bash
cp scripts/.env.example scripts/.env      # then fill in ACCESS_KEY, SECRET_KEY, CLUSTER_TOKEN
scripts/start-standalone.sh               # builds + starts one node on 127.0.0.1:7353
source /tmp/loony-standalone-run/env      # ENDPOINT/REGION/ACCESS_KEY/SECRET_KEY + rclone remote
scripts/curl-demo.sh                      # create bucket -> put -> head -> get -> list -> delete
scripts/stop-standalone.sh
```

`scripts/start-cluster.sh`/`stop-cluster.sh` do the same for a local multi-node cluster
(see [cluster.md](cluster.md)).

`curl-demo.sh` signs every request itself (pure bash + `openssl`, no SDK), so it doubles
as a readable reference for the exact signing steps this server verifies — copy its
`ls3_request` function to script your own calls.

### rclone

The env file written by the start scripts also defines an rclone remote named `loony:`
purely through `RCLONE_CONFIG_LOONY_*` variables, so no `rclone.conf` is needed:

```bash
source /tmp/loony-standalone-run/env

rclone mkdir     loony:my-bucket
rclone copy      ./photo.jpg loony:my-bucket/
rclone ls        loony:my-bucket
rclone copyto    loony:my-bucket/photo.jpg ./downloaded.jpg
rclone sync      ./local-dir loony:my-bucket/prefix
rclone deletefile loony:my-bucket/photo.jpg
rclone rmdir     loony:my-bucket
```

To configure it by hand against another server instead, set the same variables:
`RCLONE_CONFIG_LOONY_TYPE=s3`, `..._PROVIDER=Other`, `..._LIST_VERSION=2`,
`..._ENDPOINT=http://host:7353`, `..._REGION` (must match `LS3_REGION`),
`..._ACCESS_KEY_ID`, `..._SECRET_ACCESS_KEY`.

### Presigned URLs

A presigned URL needs no `Authorization` header from whoever uses it, so any plain HTTP
client can follow it. Mint one with `rclone link loony:my-bucket/photo.jpg --expire 5m`
or the web UI's "copy link" action, then:

```bash
curl -o photo.jpg "<presigned url>"
```

### AWS SDKs and the AWS CLI

Standard SDKs work with their default settings, apart from pointing them at this server
and using path-style addressing. These were tested over HTTPS with boto3 1.43, the
JavaScript SDK v3 and AWS CLI 2.36.

```python
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="https://s3.example.com",
    region_name="us-east-1",
    aws_access_key_id=ACCESS_KEY,
    aws_secret_access_key=SECRET_KEY,
    # s3v4 is only needed for presigned URLs.
    config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
)
s3.upload_file("photo.jpg", "my-bucket", "photo.jpg")
s3.download_file("my-bucket", "photo.jpg", "downloaded.jpg")
```

```js
import { S3Client } from "@aws-sdk/client-s3";

const s3 = new S3Client({
  endpoint: "https://s3.example.com",
  region: "us-east-1",
  forcePathStyle: true,
  credentials: { accessKeyId: ACCESS_KEY, secretAccessKey: SECRET_KEY },
});
```

```bash
aws --endpoint-url https://s3.example.com s3 cp photo.jpg s3://my-bucket/photo.jpg
aws --endpoint-url https://s3.example.com s3 cp s3://my-bucket/photo.jpg photo.jpg
```

`DeleteObjects` isn't supported, so delete objects one at a time.

## Run the web UI

The frontend (`frontend/`) is a Vite/React app that signs every request with SigV4
itself, directly from the browser — there's no backend-for-frontend or proxy in front of
the LS3 API.

```bash
cd frontend
npm install
cp .env.example .env.local
# edit .env.local: VITE_API_URL must point at the running server's LS3 API
#   (e.g. http://127.0.0.1:7353), VITE_REGION must match LS3_REGION
npm run dev
# open http://localhost:5173
```

Sign in with the same access key / secret key pair the server printed at startup, the
server's URL, and its region. The credentials are kept client-side (in the browser) and
used to sign every request — nothing is sent to any third party.

From there: create a bucket, drag-and-drop files to upload, list/download/delete
objects, copy a presigned link for a file. The web UI's own uploader always sends a
single `PUT` regardless of size (no chunking) — the server itself does support
multipart upload (e.g. `rclone copy` of a large file uses it automatically), see
[api-reference.md](api-reference.md).

Signed in with the root key on the same machine as the server, the **Admin** link opens
the admin panel: see [admin.md](admin.md).

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
LS3_MODE=standalone \
LS3_DATA_DIR=/tmp/loony-dev \
LS3_VOLUME_PATHS=/tmp/loony-dev/vol-a,/tmp/loony-dev/vol-b,/tmp/loony-dev/vol-c \
cargo run --bin loony-server
```

## Running more than one node

See [cluster.md](cluster.md) — and read its "what this does and doesn't mean today"
section before relying on it for anything beyond bootstrap/join/heartbeat mechanics.

## Troubleshooting

- **`server: invalid configuration: ...`** — a required env var is missing or malformed;
  the message names which one. See [configuration.md](configuration.md).
- **`--node-id / LS3_NODE_ID does not match this data directory's persisted NODE_ID`** —
  you pointed an explicit `--node-id`/`LS3_NODE_ID` at a data directory that already has a
  different identity persisted in `NODE_ID`. Drop the override, or point at an empty
  data directory.
- **SigV4 `SignatureDoesNotMatch`** — usually a region or clock-skew mismatch. Confirm
  the client's region matches `LS3_REGION` (default `us-east-1`) and the client's clock
  is close to the server's (requests more than ~15 minutes off are also rejected as
  `RequestTimeTooSkewed`).
- **`403` with no body on a bare `GET /`** — expected: every route requires a valid
  SigV4 signature, including the root path.
