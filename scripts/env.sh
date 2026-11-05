# Sourced by the other scripts in this directory -- loads scripts/.env and checks that
# the credentials a script needs are actually set. Not meant to be run directly.

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOTENV_FILE="$SCRIPTS_DIR/.env"

# Reads KEY=VALUE lines from scripts/.env into shell variables. Parsed rather than
# sourced, so a secret containing `$`, `#` or backticks is taken literally. Variables
# already set in the environment win, so one-off overrides still work. Values are not
# exported: each script hands them to the server explicitly.
load_dotenv() {
  [[ -f "$DOTENV_FILE" ]] || return 0

  local line key value
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[2]}"
    value="${BASH_REMATCH[3]}"
    if [[ "$value" =~ ^\"(.*)\"$ || "$value" =~ ^\'(.*)\'$ ]]; then
      value="${BASH_REMATCH[1]}"
    fi
    [[ -n "${!key:-}" ]] || printf -v "$key" '%s' "$value"
  done <"$DOTENV_FILE"
}

# Exits with a pointer to the fix if any of the named variables is unset or empty.
require_env() {
  local name missing=()
  for name in "$@"; do
    [[ -n "${!name:-}" ]] || missing+=("$name")
  done
  if [[ ${#missing[@]} -gt 0 ]]; then
    echo "missing required setting(s): ${missing[*]}" >&2
    if [[ -f "$DOTENV_FILE" ]]; then
      echo "set them in $DOTENV_FILE" >&2
    else
      echo "create $DOTENV_FILE from the template and fill it in:" >&2
      echo "  cp $SCRIPTS_DIR/.env.example $DOTENV_FILE" >&2
    fi
    exit 1
  fi
}

# Writes the sourceable env file for curl-demo.sh and rclone, readable only by this
# user since it holds the secret key. Usage: write_client_env <path> <endpoint>
write_client_env() {
  local path="$1" endpoint="$2"
  (
    umask 077
    rm -f "$path"
    cat >"$path" <<EOF
export ENDPOINT="$endpoint"
export REGION="$REGION"
export ACCESS_KEY=$(printf '%q' "$ACCESS_KEY")
export SECRET_KEY=$(printf '%q' "$SECRET_KEY")
# rclone remote "loony:" -- configured entirely from env, no rclone.conf needed.
export RCLONE_CONFIG_LOONY_TYPE=s3
export RCLONE_CONFIG_LOONY_PROVIDER=Other
export RCLONE_CONFIG_LOONY_LIST_VERSION=2
export RCLONE_CONFIG_LOONY_ENDPOINT="$endpoint"
export RCLONE_CONFIG_LOONY_REGION="$REGION"
export RCLONE_CONFIG_LOONY_ACCESS_KEY_ID=$(printf '%q' "$ACCESS_KEY")
export RCLONE_CONFIG_LOONY_SECRET_ACCESS_KEY=$(printf '%q' "$SECRET_KEY")
EOF
  )
}
