#!/bin/bash
# Runs hetzner-fw-updater from source with an API token read from the
# terminal, so the token is not kept in the shell history or environment.
# Takes the same arguments as the program, e.g. ./dev-run.sh check config.json
set -euo pipefail

read -rsp "Hetzner API token: " API_TOKEN
echo
if [ -z "$API_TOKEN" ]; then
    echo "no API token given" >&2
    exit 1
fi

API_TOKEN="$API_TOKEN" exec cargo run -- "$@"
