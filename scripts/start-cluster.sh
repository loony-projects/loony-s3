#!/usr/bin/env bash
# Starts a local multi-node loony-server cluster: node 1 bootstraps, every other node
# joins through it, sequentially. See docs/cluster.md for what "cluster mode" does and
# doesn't mean yet (no voter promotion/HA failover; a non-leader node can't take writes).
#
# Usage: scripts/start-cluster.sh
# Config (scripts/.env or $LS3_ENV_FILE, or environment -- see scripts/.env.example):
#   required: ACCESS_KEY, SECRET_KEY, CLUSTER_TOKEN, CLUSTER_SIZE, CLUSTER_ID, REGION,
#             DATA_ROOT, CLUSTER_STATE_DIR, CLUSTER_HOST, CLUSTER_API_PORT,
#             CLUSTER_ADMIN_PORT, CLUSTER_RPC_PORT
# Node n uses each port plus n-1.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
load_dotenv
require_env ACCESS_KEY SECRET_KEY CLUSTER_TOKEN CLUSTER_SIZE CLUSTER_ID REGION DATA_ROOT \
  CLUSTER_STATE_DIR CLUSTER_HOST CLUSTER_API_PORT CLUSTER_ADMIN_PORT CLUSTER_RPC_PORT

STATE_DIR="$CLUSTER_STATE_DIR"
ENV_FILE="$STATE_DIR/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/debug/loony-server"

if [[ "$CLUSTER_SIZE" -lt 1 ]]; then
  echo "CLUSTER_SIZE must be at least 1" >&2
  exit 1
fi

if ls "$STATE_DIR"/node*.pid >/dev/null 2>&1; then
  for pf in "$STATE_DIR"/node*.pid; do
    pid="$(cat "$pf")"
    if kill -0 "$pid" 2>/dev/null; then
      echo "already running (pid $pid, see $STATE_DIR) -- run stop-cluster.sh first" >&2
      exit 0
    fi
  done
fi

mkdir -p "$STATE_DIR"

echo "building loony-server..."
(cd "$REPO_ROOT" && cargo build --bin loony-server 2>&1 | tail -5)

# Bash's /dev/tcp pseudo-device -- no curl/nc dependency needed just to know a port
# is accepting connections yet.
wait_for_port() {
  local host="$1" port="$2"
  for _ in $(seq 1 50); do
    if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
      exec 3>&- 3<&-
      return 0
    fi
    sleep 0.2
  done
  return 1
}

declare -a BIND_ADDRS CLUSTER_ADDRS

for i in $(seq 1 "$CLUSTER_SIZE"); do
  idx=$((i - 1))
  bind_port=$((CLUSTER_API_PORT + idx))
  cluster_port=$((CLUSTER_RPC_PORT + idx))
  admin_addr="$CLUSTER_HOST:$((CLUSTER_ADMIN_PORT + idx))"
  bind_addr="$CLUSTER_HOST:$bind_port"
  cluster_addr="$CLUSTER_HOST:$cluster_port"
  BIND_ADDRS[$i]="$bind_addr"
  CLUSTER_ADDRS[$i]="$cluster_addr"
  data_dir="$DATA_ROOT/node$i"
  log_file="$STATE_DIR/node$i.log"
  pid_file="$STATE_DIR/node$i.pid"
  rm -rf "$data_dir"
  mkdir -p "$data_dir"

  if [[ "$i" -eq 1 ]]; then
    echo "starting node 1 (bootstrap) -- api $bind_addr, cluster $cluster_addr"
    LS3_MODE=cluster LS3_CLUSTER_TOKEN="$CLUSTER_TOKEN" LS3_CLUSTER_ID="$CLUSTER_ID" \
      LS3_DATA_DIR="$data_dir" LS3_BIND_ADDR="$bind_addr" LS3_ADMIN_ADDR="$admin_addr" LS3_REGION="$REGION" \
      LS3_CLUSTER_ADDR="$cluster_addr" LS3_ADVERTISE_ADDR="$cluster_addr" \
      LS3_ROOT_ACCESS_KEY="$ACCESS_KEY" LS3_ROOT_SECRET_KEY="$SECRET_KEY" \
      nohup "$BIN" --mode cluster --bootstrap >"$log_file" 2>&1 &
    echo $! >"$pid_file"

    if ! wait_for_port "$CLUSTER_HOST" "$cluster_port"; then
      echo "node 1 never came up -- see $log_file" >&2
      exit 1
    fi
  else
    seed="${CLUSTER_ADDRS[1]}"
    echo "starting node $i (join via $seed) -- api $bind_addr, cluster $cluster_addr"
    LS3_MODE=cluster LS3_CLUSTER_TOKEN="$CLUSTER_TOKEN" \
      LS3_DATA_DIR="$data_dir" LS3_BIND_ADDR="$bind_addr" LS3_ADMIN_ADDR="$admin_addr" LS3_REGION="$REGION" \
      LS3_CLUSTER_ADDR="$cluster_addr" LS3_ADVERTISE_ADDR="$cluster_addr" \
      nohup "$BIN" --mode cluster --join "$seed" >"$log_file" 2>&1 &
    echo $! >"$pid_file"

    if ! wait_for_port "$CLUSTER_HOST" "$cluster_port"; then
      echo "node $i never came up -- see $log_file" >&2
      exit 1
    fi
  fi

  if ! kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    echo "node $i exited immediately -- see $log_file" >&2
    tail -20 "$log_file" >&2
    exit 1
  fi
done

# Clients (and the rclone remote) point at node 1.
write_client_env "$ENV_FILE" "http://${BIND_ADDRS[1]}"

echo
echo "cluster up: $CLUSTER_SIZE node(s), cluster id '$CLUSTER_ID'"
for i in $(seq 1 "$CLUSTER_SIZE"); do
  echo "  node $i: api http://${BIND_ADDRS[$i]}  cluster ${CLUSTER_ADDRS[$i]}  pid $(cat "$STATE_DIR/node$i.pid")  log $STATE_DIR/node$i.log"
done
echo
echo "Only node 1 (the bootstrap node) is the metadata leader today -- writes must go"
echo "through it; reads and already-placed shard fetches work against any node."
echo
echo "curl demo: source $ENV_FILE && scripts/curl-demo.sh"
echo "rclone:    source $ENV_FILE && rclone lsd loony:"
