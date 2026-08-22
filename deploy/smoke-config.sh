#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

umask 077
"$root/deploy/generate-relay-credentials.sh" "$tmp/relay.tokens" >/dev/null
test "$(stat -c '%a' "$tmp/relay.tokens")" = 600
CLIPSYNC_RELAY_TOKEN_FILE="$tmp/relay.tokens" \
  docker compose -f "$root/deploy/docker-compose.yml" config --quiet
cargo run -q -p clipsyncd -- validate-config --config "$root/deploy/config/relay.toml"
printf '%s\n' 'relay deployment configuration smoke test passed'
