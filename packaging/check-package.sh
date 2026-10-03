#!/bin/sh
set -eu
arch=$1
version=$2
mount --make-rshared /run
artifacts=${3:-/artifacts}
package="$artifacts/gafctl_${version}_${arch}.deb"
mkdir -p /work/archive
tar -xzf "$artifacts/gafctl_${version}_linux_${arch}.tar.gz" -C /work/archive
test "$(/work/archive/gafctl --version)" = "gafctl $version"
test "$(/work/archive/gafctl-server --version)" = "gafctl-server $version"
/work/archive/gafctl server --help >/dev/null
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$package"
test "$(dpkg-query -W -f '${Architecture}' gafctl)" = "$arch"
test "$(gafctl --version)" = "gafctl $version"
test "$(gafctl-server --version)" = "gafctl-server $version"
gafctl server --help >/dev/null
test "$(stat -c '%a:%U:%G' /etc/gafctl/gafctl.env)" = 640:root:gafctl
getent passwd gafctl >/dev/null
test -f /etc/dbus-1/system.d/gafctl.conf
systemd-analyze verify /usr/lib/systemd/system/gafctl.service
printf 'synthetic secret\n' > /etc/gafctl/check-password
chmod 600 /etc/gafctl/check-password
mkdir -p /etc/systemd/system/gafctl.service.d
cat > /etc/systemd/system/gafctl.service.d/check.conf <<'UNIT'
[Service]
LoadCredential=check-password:/etc/gafctl/check-password
ExecStartPre=/usr/bin/test -r %d/check-password
UNIT
systemctl daemon-reload
systemctl enable --now gafctl.service
curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
systemctl is-active --quiet gafctl.service
identity=$(sha256sum /var/lib/gafctl/identities.json)
cat > /etc/systemd/system/gafctl.service.d/archive.conf <<'UNIT'
[Service]
ExecStart=
ExecStart=/work/archive/gafctl server --bind 127.0.0.1:8787
UNIT
systemctl daemon-reload
systemctl restart gafctl.service
curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
rm /etc/systemd/system/gafctl.service.d/archive.conf
systemctl daemon-reload
test "$(stat -c '%a:%U:%G' /var/lib/gafctl)" = 700:gafctl:gafctl
printf 'identity fixture\n' > /var/lib/gafctl/install-check
printf '# preserved configuration\n' >> /etc/gafctl/gafctl.env
systemctl restart gafctl.service
curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
test "$(sha256sum /var/lib/gafctl/identities.json)" = "$identity"
dpkg -i "$package"
grep -q 'preserved configuration' /etc/gafctl/gafctl.env
mkdir -p /work/upgrade
dpkg-deb -R "$package" /work/upgrade
sed -i "s/^Version: .*/Version: ${version}+check/" /work/upgrade/DEBIAN/control
dpkg-deb --build /work/upgrade /work/upgrade.deb
dpkg -i /work/upgrade.deb
grep -q 'preserved configuration' /etc/gafctl/gafctl.env
test -f /var/lib/gafctl/install-check
test "$(sha256sum /var/lib/gafctl/identities.json)" = "$identity"
systemctl restart gafctl.service
curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
dpkg --purge gafctl
if systemctl is-active --quiet gafctl.service; then exit 1; fi
if systemctl is-enabled --quiet gafctl.service; then exit 1; fi
test -f /var/lib/gafctl/install-check
getent passwd gafctl >/dev/null
printf '\npackage architecture, install, systemd, credentials, restart, reinstall, upgrade, purge: passed\n'
