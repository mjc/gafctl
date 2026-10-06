#!/bin/sh
set -eu
arch=$1
version=$2
ready() {
    curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
}
check_binaries() {
    test "$("$1" --version)" = "gafctl $version"
    test "$("$2" --version)" = "gafctl-server $version"
    "$1" server --help >/dev/null
}
mount --make-rshared /run
artifacts=${3:-/artifacts}
package="$artifacts/gafctl_${version}_${arch}.deb"
mkdir -p /work/archive
tar -xzf "$artifacts/gafctl_${version}_linux_${arch}.tar.gz" -C /work/archive
check_binaries /work/archive/gafctl /work/archive/gafctl-server
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$package"
test "$(dpkg-query -W -f '${Architecture}' gafctl)" = "$arch"
check_binaries gafctl gafctl-server
for name in LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html; do
    cmp "/work/archive/$name" "/usr/share/doc/gafctl/$name"
done
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
ready
systemctl is-active --quiet gafctl.service
identity=$(sha256sum /var/lib/gafctl/identities.json)
cat > /etc/systemd/system/gafctl.service.d/archive.conf <<'UNIT'
[Service]
ExecStart=
ExecStart=/work/archive/gafctl server --bind 127.0.0.1:8787
UNIT
systemctl daemon-reload
systemctl restart gafctl.service
ready
rm /etc/systemd/system/gafctl.service.d/archive.conf
systemctl daemon-reload
test "$(stat -c '%a:%U:%G' /var/lib/gafctl)" = 700:gafctl:gafctl
printf 'identity fixture\n' > /var/lib/gafctl/install-check
printf '# preserved configuration\n' >> /etc/gafctl/gafctl.env
systemctl restart gafctl.service
ready
test "$(sha256sum /var/lib/gafctl/identities.json)" = "$identity"
dpkg -i "$package"
grep -q 'preserved configuration' /etc/gafctl/gafctl.env
mkdir -p /work/upgrade
dpkg-deb -R "$package" /work/upgrade
sed -i "s/^Version: .*/Version: ${version}+check/" /work/upgrade/DEBIAN/control
dpkg-deb --build /work/upgrade /work/upgrade.deb
dpkg -i /work/upgrade.deb
grep -q 'preserved configuration' /etc/gafctl/gafctl.env
cp -p /etc/gafctl/gafctl.env /work/gafctl.env
rm /etc/gafctl/gafctl.env
sed -i "s/^Version: .*/Version: ${version}+check.1/" /work/upgrade/DEBIAN/control
dpkg-deb --build /work/upgrade /work/deleted-config-upgrade.deb
for archive in /work/upgrade.deb /work/deleted-config-upgrade.deb; do
    dpkg -i "$archive"
    test "$(dpkg-query -W -f '${Status}' gafctl)" = 'install ok installed'
    test ! -e /etc/gafctl/gafctl.env
done
cp -p /work/gafctl.env /etc/gafctl/gafctl.env
test -f /var/lib/gafctl/install-check
test "$(sha256sum /var/lib/gafctl/identities.json)" = "$identity"
systemctl restart gafctl.service
ready
dpkg --purge gafctl
if systemctl is-active --quiet gafctl.service; then exit 1; fi
if systemctl is-enabled --quiet gafctl.service; then exit 1; fi
test -f /var/lib/gafctl/install-check
getent passwd gafctl >/dev/null
printf '\npackage architecture, install, systemd, credentials, restart, reinstall, upgrade, deleted configuration, purge: passed\n'
