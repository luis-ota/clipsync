#!/usr/bin/env bash
set -Eeuo pipefail
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SERVICE="$ROOT/deploy/systemd/clipsyncd.service"
DIST=${1:-}
for file in "$SERVICE" "$ROOT/packaging/build-release.sh" "$ROOT/packaging/PKGBUILD" "$ROOT/packaging/clipsync.spec" "$ROOT/packaging/flatpak/org.clipsync.Clipsyncd.yml"; do
  test -f "$file"
done
test -x "$ROOT/packaging/build-release.sh"
test -x "$ROOT/packaging/smoke-test.sh"
grep -q '^ExecStart=clipsync-relay --config %E/clipsync/config.toml$' "$SERVICE"
grep -q '^StateDirectory=clipsync$' "$SERVICE"
grep -q '^ConfigurationDirectory=clipsync$' "$SERVICE"
! grep -Eq '/(usr|etc|var)/(local/)?bin|/etc/clipsync|/var/lib/clipsync' "$SERVICE"
grep -q '^install -m 0755' "$ROOT/packaging/build-release.sh"
deb=''
if [ -n "$DIST" ]; then
  for candidate in "$DIST"/clipsync_*_amd64.deb; do
    if [ -f "$candidate" ]; then deb="$candidate"; break; fi
  done
fi
if [ -n "$deb" ]; then
  contents=$(dpkg-deb --contents "$deb")
  grep -Eq 'usr/bin/clipsync(d|-relay)$' <<<"$contents"
  grep -Eq '^-rwxr-xr-x .*usr/bin/clipsyncd$' <<<"$contents"
  grep -Eq '^-.+etc/clipsync/config.toml$' <<<"$contents"
fi
printf '%s\n' 'packaging smoke test: passed'
