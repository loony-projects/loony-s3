# Admin panel and API

The web UI has an **Admin** section, backed by a JSON API built into `loony-server`. It's
for whoever runs the server: it sees every owner's buckets, manages access keys and
cluster nodes, and keeps an audit log.

## Reaching it

The admin API listens on `LS3_ADMIN_ADDR`, by default `127.0.0.1:7253`, and the server
refuses to start if that isn't a loopback address. It's a separate port from the LS3 API,
so publishing the LS3 API (through a tunnel, say) doesn't publish this. Don't route a
tunnel or proxy to it: it has no rate limiting and is designed to be local only.

To use the panel, run the UI on the same machine as the server:

```bash
cd frontend
npm install
npm run dev     # then open the http://localhost URL it prints
```

and sign in with the root access key and secret. The UI talks to the admin API at
`VITE_ADMIN_API_URL` (default `http://127.0.0.1:7253`), and the API only accepts browser
requests from `localhost`/`127.0.0.1` pages. From another machine, forward both ports:
`ssh -L 7253:127.0.0.1:7253 -L 5173:127.0.0.1:5173 server`.

The UI keeps sign-in details, including the secret, in the browser's `localStorage` until
you sign out, so sign out when you're done with the root key on a shared machine.

## Authentication

Requests are SigV4-signed exactly like LS3 requests (with the actual body hash, not
`UNSIGNED-PAYLOAD`), by a key with the admin flag. Presigned URLs aren't accepted. The
root credential (`LS3_ROOT_ACCESS_KEY`/`LS3_ROOT_SECRET_KEY`, or `ACCESS_KEY`/`SECRET_KEY`
in `scripts/.env`) gets the flag each time the server starts, and it's the only way to get
it: keys created in the panel are never admins.

If you change the root access key, the old one keeps working and stays an admin. Disable
or delete it under **Access keys**.

## What each section does

**Overview**: totals across every owner, and this node's view of the metadata group. Sizes
are logical object sizes, before redundancy. Computing them scans every object's metadata,
so these pages get slower as the object count grows; they only load when you open or
refresh them.

**All buckets**: every bucket with its owner and usage. Browsing a bucket shows names and
sizes only: it can't download, change or delete anything, and each look is audited.

**Access keys**: create, disable, enable and delete keys. A new key either starts a new
owner or joins an existing one (keys of the same owner share its buckets). The secret is
shown once, when the key is created. Two things are refused:

- changing the key you're signed in with, so you can't lock yourself out
- deleting an owner's only key while it has buckets, which would strand them (disable it
  instead)

Secrets are still stored unhashed in the metadata store, because SigV4 needs them to verify
signatures; the admin API just never returns them after creation.

**Nodes** (cluster mode): the node registry, with how many shards each node holds.

| Action | Effect |
| --- | --- |
| Drain | No new data is placed on the node. **Existing data stays where it is and stays readable: nothing is migrated off it**, because shard migration doesn't exist yet. Survives restarts of the node. |
| Resume | Puts a draining node back in service once it answers heartbeats. |
| Remove | Permanent: drops the node from the metadata group, and it can't rejoin with that identity. Only allowed for a draining node holding no shards, since removing one with shards would make that data unreadable. |

Since nothing moves shards yet, a node only reaches zero shards once the objects stored on
it have been deleted. Metadata voters, which today means the bootstrap node, can't be
drained or removed, since voter changes aren't implemented.

Changes must go through the metadata leader's admin API, which is node 1's in a cluster
started by `scripts/start-cluster.sh` (ports go up from 7253, one per node). Other nodes
answer read requests but refuse changes.

**Audit log**: admin changes, bucket browsing and rejected admin requests, with who, what
and the outcome. Written to `$LS3_DATA_DIR/audit.jsonl`, one JSON object per line, readable
only by the server's user. It's per node, not replicated, and not rotated. It doesn't cover
uploads and downloads through the LS3 API, and it records the claimed key on a rejected
request, which isn't proof of who sent it.

## API

All routes are under `/admin/v1`. Errors are `{"message": "..."}`.

| Route | Purpose |
| --- | --- |
| `GET /overview` | Status and totals |
| `GET /nodes` | Node registry with shard counts |
| `POST /nodes/{id}/drain`, `/resume`, `/remove` | Node lifecycle |
| `GET /buckets` | All buckets with usage |
| `GET /buckets/{name}/objects?prefix=&cursor=` | One folder level, 200 entries per page |
| `GET /credentials` | All keys, without secrets |
| `POST /credentials` | Create a key: `{}` or `{"owner_id": "..."}` |
| `PATCH /credentials/{key}` | `{"enabled": true\|false}` |
| `DELETE /credentials/{key}` | Delete a key |
| `GET /audit?limit=` | Newest entries first, up to 1000 |
