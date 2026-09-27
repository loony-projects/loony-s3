#!/usr/bin/env bash
# Starts one loony-server in standalone mode as a background process.
#
# Usage: scripts/start-standalone.sh
# Config (env, all optional): DATA_DIR, BIND_ADDR, REGION, ACCESS_KEY, SECRET_KEY
#
# State (pidfile/log/env-for-other-scripts) lives under $STATE_DIR, not the repo --
# see stop-standalone.sh to tear this back down, and curl-demo.sh to talk to it.
set -euo pipefail

DATA_DIR="${DATA_DIR:-/tmp/loony-standalone}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:9000}"
REGION="${REGION:-us-east-1}"
ACCESS_KEY="${ACCESS_KEY:-devkey}"
SECRET_KEY="${SECRET_KEY:-devsecret1234}"

STATE_DIR="${STATE_DIR:-/tmp/loony-standalone-run}"
PID_FILE="$STATE_DIR/loony-server.pid"
LOG_FILE="$STATE_DIR/loony-server.log"
ENV_FILE="$STATE_DIR/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/debug/loony-server"

if [[ -f "$PID_FILE" ]] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "already running (pid $(cat "$PID_FILE")) -- see $LOG_FILE" >&2
  exit 0
fi

mkdir -p "$STATE_DIR"

echo "building loony-server..."
(cd "$REPO_ROOT" && cargo build --bin loony-server 2>&1 | tail -5)

echo "starting loony-server (standalone) on http://$BIND_ADDR, data dir $DATA_DIR"
S3_MODE=standalone \
S3_DATA_DIR="$DATA_DIR" \
S3_BIND_ADDR="$BIND_ADDR" \
S3_REGION="$REGION" \
S3_ROOT_ACCESS_KEY="$ACCESS_KEY" \
S3_ROOT_SECRET_KEY="$SECRET_KEY" \
nohup "$BIN" --mode standalone >"$LOG_FILE" 2>&1 &
echo $! >"$PID_FILE"

# Give it a moment to either come up or fail fast, so we can report a useful error
# instead of a false "started" for e.g. a port already in use.
sleep 1
if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  echo "loony-server exited immediately -- see $LOG_FILE" >&2
  tail -20 "$LOG_FILE" >&2
  rm -f "$PID_FILE"
  exit 1
fi

cat >"$ENV_FILE" <<EOF
export ENDPOINT="http://$BIND_ADDR"
export REGION="$REGION"
export ACCESS_KEY="$ACCESS_KEY"
export SECRET_KEY="$SECRET_KEY"
EOF

echo "started (pid $(cat "$PID_FILE"))"
echo "  endpoint:    http://$BIND_ADDR"
echo "  access key:  $ACCESS_KEY"
echo "  secret key:  $SECRET_KEY"
echo "  log:         $LOG_FILE"
echo
echo "AWS CLI:   aws --endpoint-url http://$BIND_ADDR s3 ls"
echo "curl demo: source $ENV_FILE && scripts/curl-demo.sh"
