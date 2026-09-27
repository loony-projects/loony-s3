#!/usr/bin/env bash
# Drives loony-server's LS3 API with plain curl -- no CLI, no SDK. Every request
# needs a SigV4 signature, so this script implements that signing (header-based auth)
# itself in bash + openssl, then walks through a full
# create-bucket -> put -> head -> get -> list -> delete -> delete-bucket sequence.
#
# Requires: bash, curl, openssl. ASCII bucket/key names only (the URI-encoding here
# doesn't handle multi-byte UTF-8).
#
# Usage:
#   scripts/start-standalone.sh
#   source /tmp/loony-standalone-run/env   # sets ENDPOINT/REGION/ACCESS_KEY/SECRET_KEY
#   scripts/curl-demo.sh
#
# Config (env, all optional): ENDPOINT, REGION, ACCESS_KEY, SECRET_KEY, BUCKET, OBJECT_KEY
set -euo pipefail

ENDPOINT="${ENDPOINT:-http://127.0.0.1:9000}"
REGION="${REGION:-us-east-1}"
ACCESS_KEY="${ACCESS_KEY:-devkey}"
SECRET_KEY="${SECRET_KEY:-devsecret1234}"
BUCKET="${BUCKET:-curl-demo-bucket}"
OBJECT_KEY="${OBJECT_KEY:-hello.txt}"

# Wire-protocol literals fixed by the SigV4 spec -- every standard client signs with
# these exact bytes, so they can't be renamed without breaking compatibility. This is
# the only place they're spelled out (mirrors crates/auth/src/canonical.rs).
readonly SIGNING_ALGORITHM="AWS4-HMAC-SHA256"
readonly SIGNING_KEY_PREFIX="AWS4"
readonly SCOPE_TERMINATOR="aws4_request"
readonly SERVICE="s3"

HOST="${ENDPOINT#http://}"
HOST="${HOST#https://}"

# ---------------------------------------------------------------------------
# SigV4 primitives (mirrors crates/auth/src/canonical.rs and verify.rs exactly --
# see those for the reference implementation this is signing against).
# ---------------------------------------------------------------------------

sha256_hex() {
  # Hashes stdin.
  openssl dgst -sha256 -r | awk '{print $1}'
}

hmac_sha256_hex() {
  # $1: hex-encoded key. Hashes stdin, returns hex digest (itself usable as the next
  # step's hex key -- HMAC chaining never needs to go back through ASCII).
  openssl dgst -sha256 -mac HMAC -macopt "hexkey:$1" -r | awk '{print $1}'
}

str_to_hex() {
  printf '%s' "$1" | od -An -tx1 | tr -d ' \n'
}

# SigV4's URI-encode: unreserved chars (A-Za-z0-9-._~) pass through, everything else
# becomes %XX (uppercase hex); '/' passes through unless $2=true. ASCII-only.
uri_encode() {
  local input="$1" encode_slash="${2:-false}" out="" c i len
  len=${#input}
  for (( i = 0; i < len; i++ )); do
    c="${input:i:1}"
    if [[ "$c" =~ [A-Za-z0-9._~-] ]]; then
      out+="$c"
    elif [[ "$c" == "/" && "$encode_slash" == "false" ]]; then
      out+="$c"
    else
      printf -v hex '%%%02X' "'$c"
      out+="$hex"
    fi
  done
  printf '%s' "$out"
}

canonical_query_string() {
  # $1: raw "k=v&k2=v2" (already unencoded/simple in this script's own use) -- sorts
  # by key the same way canonical_query_string() in canonical.rs does.
  local raw="$1"
  [[ -z "$raw" ]] && return 0
  local IFS='&'
  local -a pairs=($raw)
  IFS=$'\n' pairs=($(sort <<<"${pairs[*]}"))
  local out=""
  for p in "${pairs[@]}"; do
    [[ -n "$out" ]] && out+="&"
    out+="$p"
  done
  printf '%s' "$out"
}

# Prints a fully-signed curl invocation's worth of headers for one request, then runs
# it. $1 method, $2 path (already the canonical, slash-preserving path this server
# expects, e.g. "/bucket/key"), $3 raw query string (no leading '?', may be empty),
# $4 body file (may be empty for no body). Extra curl args follow as $5+.
ls3_request() {
  local method="$1" path="$2" query="$3" body_file="${4:-}"
  shift 4 || shift $#
  local extra_curl_args=("$@")

  local amz_date date_stamp payload_hash
  amz_date="$(date -u +"%Y%m%dT%H%M%SZ")"
  date_stamp="${amz_date:0:8}"

  if [[ -n "$body_file" ]]; then
    payload_hash="$(sha256_hex <"$body_file")"
  else
    payload_hash="$(printf '' | sha256_hex)"
  fi

  local canonical_uri
  canonical_uri="$(uri_encode "$path" false)"
  [[ -z "$canonical_uri" ]] && canonical_uri="/"

  local canonical_qs
  canonical_qs="$(canonical_query_string "$query")"

  local signed_headers="host;x-amz-content-sha256;x-amz-date"
  local canonical_headers
  printf -v canonical_headers 'host:%s\nx-amz-content-sha256:%s\nx-amz-date:%s\n' \
    "$HOST" "$payload_hash" "$amz_date"

  local canonical_request
  printf -v canonical_request '%s\n%s\n%s\n%s\n%s\n%s' \
    "$method" "$canonical_uri" "$canonical_qs" "$canonical_headers" "$signed_headers" "$payload_hash"

  local hashed_canonical_request
  hashed_canonical_request="$(printf '%s' "$canonical_request" | sha256_hex)"

  local credential_scope="$date_stamp/$REGION/$SERVICE/$SCOPE_TERMINATOR"
  local string_to_sign
  printf -v string_to_sign '%s\n%s\n%s\n%s' \
    "$SIGNING_ALGORITHM" "$amz_date" "$credential_scope" "$hashed_canonical_request"

  local k_secret_hex k_date k_region k_service k_signing signature
  k_secret_hex="$(str_to_hex "$SIGNING_KEY_PREFIX$SECRET_KEY")"
  k_date="$(printf '%s' "$date_stamp" | hmac_sha256_hex "$k_secret_hex")"
  k_region="$(printf '%s' "$REGION" | hmac_sha256_hex "$k_date")"
  k_service="$(printf '%s' "$SERVICE" | hmac_sha256_hex "$k_region")"
  k_signing="$(printf '%s' "$SCOPE_TERMINATOR" | hmac_sha256_hex "$k_service")"
  signature="$(printf '%s' "$string_to_sign" | hmac_sha256_hex "$k_signing")"

  local authorization="$SIGNING_ALGORITHM Credential=$ACCESS_KEY/$credential_scope, SignedHeaders=$signed_headers, Signature=$signature"

  local url="$ENDPOINT$path"
  [[ -n "$query" ]] && url="$url?$query"

  echo "--- $method $path${query:+?$query}"

  local -a curl_args=(
    -sS -D /tmp/loony-curl-demo-headers.txt -w '\nHTTP %{http_code}\n'
    -H "Host: $HOST"
    -H "x-amz-date: $amz_date"
    -H "x-amz-content-sha256: $payload_hash"
    -H "Authorization: $authorization"
  )
  if [[ "$method" == "HEAD" ]]; then
    # `-X HEAD` alone only changes the request line's method text -- it doesn't tell
    # curl to skip reading a body, so it hangs waiting for bytes a HEAD response
    # correctly never sends. `--head`/`-I` is the flag that actually does that.
    curl_args+=(--head)
  else
    curl_args+=(-X "$method")
  fi
  [[ -n "$body_file" ]] && curl_args+=(--data-binary "@$body_file")
  curl_args+=("${extra_curl_args[@]}")
  curl_args+=("$url")

  curl "${curl_args[@]}"
  echo "  (response headers in /tmp/loony-curl-demo-headers.txt)"
  echo
}

# ---------------------------------------------------------------------------
# Demo sequence
# ---------------------------------------------------------------------------

echo "endpoint: $ENDPOINT   region: $REGION   access key: $ACCESS_KEY"
echo

echo "== CreateBucket =="
ls3_request PUT "/$BUCKET" ""

echo "== HeadBucket =="
ls3_request HEAD "/$BUCKET" ""

echo "== PutObject =="
body_file="$(mktemp)"
trap 'rm -f "$body_file" "$downloaded_file"' EXIT
printf 'hello from curl-demo.sh, %s\n' "$(date -u)" >"$body_file"
ls3_request PUT "/$BUCKET/$OBJECT_KEY" "" "$body_file" -H "Content-Type: text/plain"

echo "== HeadObject =="
ls3_request HEAD "/$BUCKET/$OBJECT_KEY" ""

echo "== GetObject =="
downloaded_file="$(mktemp)"
ls3_request GET "/$BUCKET/$OBJECT_KEY" "" "" -o "$downloaded_file"
if diff -q "$body_file" "$downloaded_file" >/dev/null; then
  echo "  downloaded body matches what was uploaded"
else
  echo "  MISMATCH -- uploaded and downloaded bodies differ" >&2
fi
echo

echo "== ListObjectsV2 =="
ls3_request GET "/$BUCKET" "list-type=2"

echo "== DeleteObject =="
ls3_request DELETE "/$BUCKET/$OBJECT_KEY" ""

echo "== DeleteBucket =="
ls3_request DELETE "/$BUCKET" ""

echo "done"
