# Configuration reference

Every setting `loony-server` accepts, as an environment variable and (where one exists) the
equivalent CLI flag. CLI flags always win over their environment variable when both are
given. See `crates/server/src/config.rs` for the authoritative source — this file mirrors
it.

## Common (both modes)

| Env var | CLI flag | Default | Required | Notes |
|---|---|---|---|---|
| `LS3_MODE` | `--mode standalone\|cluster` | — | yes | Selects standalone vs. cluster mode. |
| `LS3_DATA_DIR` | — | — | yes | Root of this node's persistent state: `NODE_ID`, `meta.redb`, and (unless `LS3_VOLUME_PATHS` is set) `volumes/vol-0`. |
| `LS3_BIND_ADDR` | — | `0.0.0.0:9000` | no | Where the public LS3 API listens. |
| `LS3_ADMIN_ADDR` | — | `0.0.0.0:9001` | no | Reserved for an admin API. Parsed and logged at startup but **nothing is bound to it yet** — `loony-admin`/`loony-admin-cli` are stubs. |
| `LS3_REGION` | — | `us-east-1` | no | SigV4 region scope this server verifies requests against. Clients must match it. |
| `LS3_VOLUME_PATHS` | — | `<LS3_DATA_DIR>/volumes/vol-0` | no | Comma-separated local volume directories. More volumes → a wider erasure-coding scheme becomes available (see [api-reference.md](api-reference.md)). |
| `LS3_NODE_ID` | `--node-id <uuid>` | auto-generated on first start | no | Pins this process to a specific persistent node identity. Must match what's already in `$LS3_DATA_DIR/NODE_ID` if that file exists — the server refuses to start otherwise. |
| `LS3_ROOT_ACCESS_KEY` + `LS3_ROOT_SECRET_KEY` | — | dev-derived credential | no (but see below) | The root SigV4 credential. Without both set, the server derives a deterministic (and therefore guessable) credential from the node's identity and logs it loudly — fine for local development, not for anything reachable by anyone else. |

## Cluster mode only

These apply only when `LS3_MODE=cluster`. Standalone mode ignores all of them.

| Env var | CLI flag | Default | Required | Notes |
|---|---|---|---|---|
| `LS3_ADVERTISE_ADDR` | — | — | yes | The address *other* nodes should use to reach this one's internal RPC port. Must be resolvable from every other node, not just `localhost`. |
| `LS3_CLUSTER_ADDR` | — | `0.0.0.0:9100` | no | Where this node's internal node-to-node RPC server listens (shard transfer, cluster join, heartbeat, Raft RPCs). Never exposed to LS3 clients. |
| `LS3_CLUSTER_ID` | `--cluster-id <id>` | — | yes when bootstrapping | The cluster's identity. Required (via either form) when `--bootstrap` is set; optional but validated against the seed's actual cluster id when joining. |
| `LS3_BOOTSTRAP` | `--bootstrap` | — | exactly one of this or join | Starts a brand-new cluster with this node as its first member. Requires an empty data directory. |
| `LS3_JOIN` | `--join <seed-addr>` | — | exactly one of this or bootstrap | Joins an existing cluster through the given seed node's advertised address. |
| `LS3_CLUSTER_TOKEN` | — | derived from `LS3_CLUSTER_ID` | no (but see below) | Shared bearer token authenticating the internal RPC transport between nodes. Without it, every node derives the same token from the cluster id (which every node needs to know anyway) — fine for local development, not for production (the architecture doc commits to mTLS for that; this token is the interim "simpler credentials" path it explicitly allows). |

Exactly one of `--bootstrap`/`LS3_BOOTSTRAP` or `--join`/`LS3_JOIN` must be given in
cluster mode — the server refuses to start with both or neither.

## Data directory layout

```
$LS3_DATA_DIR/
├── NODE_ID           # this node's persistent identity, minted once
├── meta.redb         # metadata: buckets, objects, credentials, node registry,
│                      # cluster identity — a real openraft-backed store (Phase 8)
└── volumes/
    └── vol-0/         # one directory per LS3_VOLUME_PATHS entry; shard bytes
```

Deleting `meta.redb` or a volume directory is destructive and not recoverable by this
server (no external backup mechanism exists yet) — treat `$LS3_DATA_DIR` as the single
source of truth for that node's data.

## Logging

Currently fixed at `info` level, human-readable (not JSON) output — `loony-server`'s
`main.rs` always calls `init_tracing(LoggingConfig::default())`. There is no environment
variable to change this yet, even though the underlying `loony-observability::LoggingConfig`
type supports both a filter directive string and a JSON-output switch (it's just not
wired to anything in `main` yet). If you need different log levels or JSON output today,
edit that call site directly.
