#!/usr/bin/env bash
# Stops the standalone loony-server started by start-standalone.sh. Idempotent: safe
# to run when nothing is running.
set -euo pipefail

STATE_DIR="${STATE_DIR:-/tmp/loony-standalone-run}"
PID_FILE="$STATE_DIR/loony-server.pid"

if [[ ! -f "$PID_FILE" ]]; then
  echo "not running (no pidfile at $PID_FILE)"
  exit 0
fi

pid="$(cat "$PID_FILE")"
if ! kill -0 "$pid" 2>/dev/null; then
  echo "not running (stale pidfile for pid $pid)"
  rm -f "$PID_FILE"
  exit 0
fi

echo "stopping loony-server (pid $pid)..."
kill "$pid"
for _ in $(seq 1 20); do
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$pid" 2>/dev/null; then
  echo "still running after graceful stop, forcing..." >&2
  kill -9 "$pid" 2>/dev/null || true
fi

rm -f "$PID_FILE"
echo "stopped"
