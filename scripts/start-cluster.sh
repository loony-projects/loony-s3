#!/usr/bin/env bash
# Starts a local multi-node loony-server cluster: node 1 bootstraps, every other node
# joins through it, sequentially. See docs/cluster.md for what "cluster mode" does and
# doesn't mean yet (no voter promotion/HA failover; a non-leader node can't take writes).
#
# Usage: scripts/start-cluster.sh
# Config (env, all optional): CLUSTER_SIZE (default 3), DATA_ROOT, REGION,
#   CLUSTER_ID, CLUSTER_TOKEN, ACCESS_KEY, SECRET_KEY
set -euo pipefail

CLUSTER_SIZE="${CLUSTER_SIZE:-3}"
DATA_ROOT="${DATA_ROOT:-/tmp/loony-cluster}"
REGION="${REGION:-us-east-1}"
CLUSTER_ID="${CLUSTER_ID:-devcluster}"
CLUSTER_TOKEN="${CLUSTER_TOKEN:-devclustertoken}"
ACCESS_KEY="${ACCESS_KEY:-devkey}"
SECRET_KEY="${SECRET_KEY:-devsecret1234}"

STATE_DIR="${STATE_DIR:-/tmp/loony-cluster-run}"
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
  bind_port=$((9000 + idx))
  cluster_port=$((9100 + idx))
  bind_addr="127.0.0.1:$bind_port"
  cluster_addr="127.0.0.1:$cluster_port"
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
      LS3_DATA_DIR="$data_dir" LS3_BIND_ADDR="$bind_addr" LS3_REGION="$REGION" \
      LS3_CLUSTER_ADDR="$cluster_addr" LS3_ADVERTISE_ADDR="$cluster_addr" \
      LS3_ROOT_ACCESS_KEY="$ACCESS_KEY" LS3_ROOT_SECRET_KEY="$SECRET_KEY" \
      nohup "$BIN" --mode cluster --bootstrap >"$log_file" 2>&1 &
    echo $! >"$pid_file"

    if ! wait_for_port 127.0.0.1 "$cluster_port"; then
      echo "node 1 never came up -- see $log_file" >&2
      exit 1
    fi
  else
    seed="${CLUSTER_ADDRS[1]}"
    echo "starting node $i (join via $seed) -- api $bind_addr, cluster $cluster_addr"
    LS3_MODE=cluster LS3_CLUSTER_TOKEN="$CLUSTER_TOKEN" \
      LS3_DATA_DIR="$data_dir" LS3_BIND_ADDR="$bind_addr" LS3_REGION="$REGION" \
      LS3_CLUSTER_ADDR="$cluster_addr" LS3_ADVERTISE_ADDR="$cluster_addr" \
      nohup "$BIN" --mode cluster --join "$seed" >"$log_file" 2>&1 &
    echo $! >"$pid_file"

    if ! wait_for_port 127.0.0.1 "$cluster_port"; then
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

cat >"$ENV_FILE" <<EOF
export ENDPOINT="http://${BIND_ADDRS[1]}"
export REGION="$REGION"
export ACCESS_KEY="$ACCESS_KEY"
export SECRET_KEY="$SECRET_KEY"
# rclone remote "loony:" (node 1) -- configured entirely from env, no rclone.conf needed.
export RCLONE_CONFIG_LOONY_TYPE=s3
export RCLONE_CONFIG_LOONY_PROVIDER=Other
export RCLONE_CONFIG_LOONY_LIST_VERSION=2
export RCLONE_CONFIG_LOONY_ENDPOINT="http://${BIND_ADDRS[1]}"
export RCLONE_CONFIG_LOONY_REGION="$REGION"
export RCLONE_CONFIG_LOONY_ACCESS_KEY_ID="$ACCESS_KEY"
export RCLONE_CONFIG_LOONY_SECRET_ACCESS_KEY="$SECRET_KEY"
EOF

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
