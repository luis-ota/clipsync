#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=${1:-}

case "$VERSION" in
  ''|*[!0-9.]*|.*|*.|*..*)
    printf 'usage: %s MAJOR.MINOR.PATCH\n' "$0" >&2
    exit 2
    ;;
esac

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'invalid version: %s\n' "$VERSION" >&2
  exit 2
fi

sed -i -E "0,/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/s//version = \"$VERSION\"/" \
  "$ROOT/Cargo.toml"
sed -i -E "s/orElse\(\"[0-9]+\.[0-9]+\.[0-9]+\"\)/orElse(\"$VERSION\")/" \
  "$ROOT/android/app/build.gradle.kts"

# Keep Cargo.lock aligned without compiling unrelated platform-specific crates.
cargo update --workspace --offline --manifest-path "$ROOT/Cargo.toml"

printf 'release version set to %s\n' "$VERSION"
