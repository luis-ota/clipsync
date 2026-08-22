#!/bin/sh
set -eu

output=${1:-relay.tokens}
account=${2:-$(uuidgen)}
device=${3:-$(uuidgen)}
session=${4:-$(uuidgen)}
group=${5:-$(uuidgen)}

umask 077
token=$(openssl rand -hex 32)
tmp=$(mktemp "${output}.XXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
printf '%s %s %s %s %s\n' "$token" "$account" "$device" "$session" "$group" >"$tmp"
chmod 600 "$tmp"
mv -f "$tmp" "$output"
printf 'credentials written to %s (mode 0600)\n' "$output" >&2
