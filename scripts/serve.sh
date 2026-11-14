#!/usr/bin/env bash
# Runs the release loony-server in the foreground, for a service manager to supervise.
# Build it first: cargo build --release --bin loony-server
#
# Config (scripts/.env or environment -- see scripts/.env.example):
#   required: ACCESS_KEY, SECRET_KEY, DATA_DIR, BIND_ADDR, REGION
#   optional: any LS3_* setting, such as LS3_ADMIN_ADDR, LS3_OIDC_*, LS3_UI_* or LS3_BILLING_*
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
load_dotenv
require_env ACCESS_KEY SECRET_KEY DATA_DIR BIND_ADDR REGION
export_ls3_settings

export LS3_MODE=standalone
export LS3_DATA_DIR="$DATA_DIR"
export LS3_BIND_ADDR="$BIND_ADDR"
export LS3_REGION="$REGION"
export LS3_ROOT_ACCESS_KEY="$ACCESS_KEY"
export LS3_ROOT_SECRET_KEY="$SECRET_KEY"

exec "$SCRIPTS_DIR/../target/release/loony-server" --mode standalone
