#!/usr/bin/env bash
# Kept only because the live service (loony-s3.service) runs this path. Everything else
# is in ./loony: see `./loony help`.
exec "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../loony" serve
