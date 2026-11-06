#!/usr/bin/env bash
# Runs the release loony-server in the foreground, for a service manager to supervise.
# Build it first: cargo build --release --bin loony-server
#
# Config (scripts/.env or environment -- see scripts/.env.example):
#   required: ACCESS_KEY, SECRET_KEY, DATA_DIR
#   optional: BIND_ADDR, REGION, the LS3_OIDC_* sign-in settings, and LS3_UI_DIR
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
load_dotenv
# DATA_DIR has no default here: the start scripts' /tmp fallback would lose real data.
require_env ACCESS_KEY SECRET_KEY DATA_DIR

export LS3_MODE=standalone
export LS3_DATA_DIR="$DATA_DIR"
export LS3_BIND_ADDR="${BIND_ADDR:-127.0.0.1:7353}"
export LS3_REGION="${REGION:-us-east-1}"
export LS3_ROOT_ACCESS_KEY="$ACCESS_KEY"
export LS3_ROOT_SECRET_KEY="$SECRET_KEY"

# Sign in with Loony, off unless these are set. See docs/configuration.md.
for setting in LS3_OIDC_ISSUER LS3_OIDC_INTERNAL_URL LS3_OIDC_CLIENT_ID LS3_OIDC_CLIENT_SECRET \
  LS3_OIDC_REDIRECT_URIS LS3_CONSOLE_SESSION_TTL_SECS LS3_TRUSTED_IP_HEADER \
  LS3_UI_DIR LS3_UI_ADDR; do
  [[ -z "${!setting:-}" ]] || export "${setting?}"
done

exec "$SCRIPTS_DIR/../target/release/loony-server" --mode standalone
