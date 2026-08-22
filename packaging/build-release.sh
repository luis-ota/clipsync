#!/usr/bin/env bash
set -Eeuo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-"$ROOT/dist"}
VERSION=${CLIPSYNC_VERSION:-$(awk -F'"' '/^version =/{print $2; exit}' "$ROOT/Cargo.toml")}
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || date -u +%s)}

rm -rf "$OUT"
mkdir -p "$OUT/stage/usr/bin" "$OUT/stage/usr/lib/systemd/system" \
  "$OUT/stage/etc/clipsync" "$OUT/stage/usr/share/doc/clipsync" \
  "$OUT/stage/usr/share/licenses/clipsync"

cargo build --locked --release --manifest-path "$ROOT/Cargo.toml" -p clipsyncd -p clipsync-relay
install -m 0755 "$ROOT/target/release/clipsyncd" "$OUT/stage/usr/bin/clipsyncd"
install -m 0755 "$ROOT/target/release/clipsync-relay" "$OUT/stage/usr/bin/clipsync-relay"
install -m 0644 "$ROOT/deploy/systemd/clipsyncd.service" "$OUT/stage/usr/lib/systemd/system/"
install -m 0644 "$ROOT/deploy/config/relay.toml" "$OUT/stage/etc/clipsync/config.toml"
install -m 0644 "$ROOT/README.md" "$ROOT/docs/DEPLOY.md" "$OUT/stage/usr/share/doc/clipsync/"
install -m 0644 "$ROOT/LICENSE" "$OUT/stage/usr/share/licenses/clipsync/"

tar --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner \
  --exclude='./target' --exclude='./dist' --exclude='./.git' \
  -C "$ROOT" -czf "$OUT/clipsync-${VERSION}.tar.gz" .

if command -v dpkg-deb >/dev/null 2>&1; then
  mkdir -p "$OUT/deb/DEBIAN"
  cp -a "$OUT/stage/." "$OUT/deb/"
  cat >"$OUT/deb/DEBIAN/control" <<EOF
Package: clipsync
Version: ${VERSION}
Section: net
Priority: optional
Architecture: amd64
Maintainer: clipsync contributors
Description: secure clipboard synchronization daemon and relay
EOF
  cat >"$OUT/deb/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ] && command -v systemctl >/dev/null 2>&1; then
  systemctl daemon-reload || true
fi
EOF
  chmod 0755 "$OUT/deb/DEBIAN/postinst"
  dpkg-deb --build --root-owner-group "$OUT/deb" "$OUT/clipsync_${VERSION}_amd64.deb" >/dev/null
else
  printf '%s\n' "dpkg-deb unavailable: skipped .deb (not an error for this target)" >&2
fi

if command -v rpmbuild >/dev/null 2>&1; then
  mkdir -p "$OUT/rpm/BUILD" "$OUT/rpm/BUILDROOT" "$OUT/rpm/RPMS" "$OUT/rpm/SOURCES" "$OUT/rpm/SPECS"
  cp "$OUT/clipsync-${VERSION}.tar.gz" "$OUT/rpm/SOURCES/"
  sed "s/@VERSION@/${VERSION}/g" "$ROOT/packaging/clipsync.spec" >"$OUT/rpm/SPECS/clipsync.spec"
  rpmbuild --define "_topdir $OUT/rpm" -bb "$OUT/rpm/SPECS/clipsync.spec" >/dev/null
  find "$OUT/rpm/RPMS" -type f -name '*.rpm' -exec cp {} "$OUT/" \;
else
  printf '%s\n' "rpmbuild unavailable: skipped .rpm (not an error for this target)" >&2
fi

rm -rf "$OUT/deb" "$OUT/rpm" "$OUT/stage"
printf '%s\n' "release artifacts written to $OUT"
