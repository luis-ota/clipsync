#!/bin/sh
set -eu

REPO="${CLIPSYNC_REPO:-luis-ota/clipsync}"
INSTALL_DIR="${CLIPSYNC_INSTALL_DIR:-$HOME/.local/bin}"
API="https://api.github.com/repos/$REPO/releases/latest"

command -v curl >/dev/null 2>&1 || { echo "clipsync: curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "clipsync: tar is required" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
  Linux:x86_64|Linux:amd64) asset_os=linux; asset_arch=x86_64 ;;
  Darwin:x86_64|Darwin:arm64) asset_os=macos; asset_arch="$arch" ;;
  *) echo "clipsync: unsupported platform $os/$arch; see https://github.com/$REPO/releases" >&2; exit 1 ;;
esac

release_json=$(curl -fsSL "$API")
tag=${CLIPSYNC_VERSION:-$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\(v[0-9][^"]*\)".*/\1/p' | head -n 1)}
[ -n "$tag" ] || { echo "clipsync: could not determine latest release" >&2; exit 1; }
version=${tag#v}

if [ "$asset_os" = linux ]; then
  asset="clipsync-${version}-linux-${asset_arch}.tar.gz"
  url="https://github.com/$REPO/releases/download/$tag/$asset"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM
  curl -fL --retry 3 -o "$tmp/$asset" "$url"
  curl -fL --retry 3 -o "$tmp/SHA256SUMS" \
    "https://github.com/$REPO/releases/download/$tag/SHA256SUMS"
  awk -v file="$asset" '$2 == file || $2 ~ "/" file "$" { print $1 "  " file }' "$tmp/SHA256SUMS" \
    | (cd "$tmp" && sha256sum -c -)
  mkdir -p "$tmp/unpacked" "$INSTALL_DIR"
  tar -xzf "$tmp/$asset" -C "$tmp/unpacked"
  install -m 0755 "$tmp/unpacked/usr/bin/clipsyncd" "$INSTALL_DIR/clipsyncd"
  install -m 0755 "$tmp/unpacked/usr/bin/clipsync-relay" "$INSTALL_DIR/clipsync-relay"
  echo "clipsync $version installed in $INSTALL_DIR"
  case ":${PATH:-}:" in *:"$INSTALL_DIR":*) ;; *) echo "Add $INSTALL_DIR to PATH to use clipsyncd." ;; esac
else
  echo "Automatic macOS installation is not available from a POSIX shell yet." >&2
  echo "Download the macOS asset from https://github.com/$REPO/releases/tag/$tag" >&2
  exit 1
fi
