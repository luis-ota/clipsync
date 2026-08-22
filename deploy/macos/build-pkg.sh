#!/bin/sh
set -eu
: "${1:?usage: build-pkg.sh RELEASE_DIR OUTPUT_PKG}"
: "${2:?usage: build-pkg.sh RELEASE_DIR OUTPUT_PKG}"
release_dir=$1
output=$2
app="$release_dir/ClipSync.app"
test -x "$app/Contents/MacOS/clipsync-client"
test -f "$app/Contents/Info.plist"
pkgbuild --root "$release_dir" --identifier dev.clipsync.desktop --version "${CLIPSYNC_VERSION:-0.1.0}" --install-location /Applications "$output"
