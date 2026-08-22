Name: clipsync
Version: @VERSION@
Release: 1%{?dist}
Summary: Secure clipboard synchronization daemon and relay
License: Apache-2.0 OR MIT
URL: https://github.com/luis-ota/clipsync
Source0: %{name}-%{version}.tar.gz
Requires: systemd

%description
Secure clipboard synchronization daemon and TLS WebSocket relay.

%prep
%setup -q

%build
cargo build --locked --release -p clipsyncd -p clipsync-relay

%install
mkdir -p %{buildroot}/usr/bin %{buildroot}/usr/lib/systemd/system %{buildroot}/etc/clipsync
install -m 0755 target/release/clipsyncd %{buildroot}/usr/bin/clipsyncd
install -m 0755 target/release/clipsync-relay %{buildroot}/usr/bin/clipsync-relay
install -m 0644 deploy/systemd/clipsyncd.service %{buildroot}/usr/lib/systemd/system/
install -m 0644 deploy/config/relay.toml %{buildroot}/etc/clipsync/config.toml
mkdir -p %{buildroot}/usr/share/doc/clipsync %{buildroot}/usr/share/licenses/clipsync
install -m 0644 README.md docs/DEPLOY.md %{buildroot}/usr/share/doc/clipsync/
install -m 0644 LICENSE %{buildroot}/usr/share/licenses/clipsync/

%post
%systemd_post clipsyncd.service

%preun
%systemd_preun clipsyncd.service

%postun
%systemd_postun_with_restart clipsyncd.service

%files
%config(noreplace) /etc/clipsync/config.toml
/usr/bin/clipsyncd
/usr/bin/clipsync-relay
/usr/lib/systemd/system/clipsyncd.service
/usr/share/doc/clipsync/README.md
/usr/share/doc/clipsync/DEPLOY.md
/usr/share/licenses/clipsync/LICENSE
