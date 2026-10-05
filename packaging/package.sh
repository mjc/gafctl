#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
arch=${1:-$(dpkg --print-architecture)}
case "$arch" in amd64|arm64) ;; *) echo "Unsupported package architecture: $arch" >&2; exit 1 ;; esac
version=$(packaging/version.sh)
case "$arch" in amd64) machine=3e00 ;; arm64) machine=b700 ;; esac
for executable in gafctl gafctl-server; do
    binary="target/release/$executable"
    header=$(od -An -N6 -tx1 "$binary" | tr -d ' \n')
    actual_machine=$(od -An -j18 -N2 -tx1 "$binary" | tr -d ' \n')
    if [ "$header" != 7f454c460201 ] || [ "$actual_machine" != "$machine" ]; then
        echo "Expected a Linux 64-bit little-endian $arch ELF executable: $binary" >&2
        exit 1
    fi
    if [ "$("$binary" --version)" != "$executable $version" ]; then
        echo "Binary version does not match release $version: $binary" >&2
        exit 1
    fi
done
root="target/packages/gafctl_${version}_${arch}"
mkdir -p "$root/usr/share/doc/gafctl" "$root/DEBIAN" "$root/usr/bin" "$root/usr/lib/systemd/system" "$root/etc/gafctl" "$root/etc/dbus-1/system.d" dist
install -m 644 LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt "$root/usr/share/doc/gafctl/"
install -m 755 target/release/gafctl target/release/gafctl-server "$root/usr/bin/"
install -m 644 packaging/systemd/gafctl.service "$root/usr/lib/systemd/system/"
install -m 640 packaging/gafctl.env "$root/etc/gafctl/"
install -m 644 packaging/dbus/gafctl.conf "$root/etc/dbus-1/system.d/"
install -m 755 packaging/debian/postinst packaging/debian/prerm packaging/debian/postrm "$root/DEBIAN/"
printf '/etc/gafctl/gafctl.env\n/etc/dbus-1/system.d/gafctl.conf\n' > "$root/DEBIAN/conffiles"
cat > "$root/DEBIAN/control" <<EOF
Package: gafctl
Version: $version
Architecture: $arch
Maintainer: mjc <mjc@users.noreply.github.com>
Depends: libc6 (>= 2.36), libgcc-s1, libdbus-1-3, ca-certificates, adduser, dbus
Recommends: bluez
Section: utils
Priority: optional
Homepage: https://github.com/mjc/gafctl
Description: GAF Master Flow attic fan proxy and controller
 HTTP and MQTT service with a CLI and Home Assistant integration.
EOF
dpkg-deb --root-owner-group --build "$root" "dist/gafctl_${version}_${arch}.deb"
tar -czf "dist/gafctl_${version}_linux_${arch}.tar.gz" -C target/release gafctl gafctl-server -C ../.. LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt
