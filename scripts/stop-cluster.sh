#!/usr/bin/env bash
# Stops every node started by start-cluster.sh. Idempotent: safe to run when nothing
# is running.
set -euo pipefail

STATE_DIR="${STATE_DIR:-/tmp/loony-cluster-run}"

shopt -s nullglob
pid_files=("$STATE_DIR"/node*.pid)
shopt -u nullglob

if [[ ${#pid_files[@]} -eq 0 ]]; then
  echo "not running (no pidfiles in $STATE_DIR)"
  exit 0
fi

for pf in "${pid_files[@]}"; do
  name="$(basename "$pf" .pid)"
  pid="$(cat "$pf")"
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "$name: not running (stale pidfile for pid $pid)"
    rm -f "$pf"
    continue
  fi
  echo "stopping $name (pid $pid)..."
  kill "$pid"
  for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "$name still running after graceful stop, forcing..." >&2
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pf"
done

echo "stopped"
