#!/bin/sh
set -eu
privilege=${GAFCTL_CHECK_SUDO:-sudo}
fixture=/run/gafctl-nix-check
state=/var/lib/gafctl-nix-check
unit=gafctl-nix-check.service
broker=gafctl-nix-check-broker.service
username=test-account
password=synthetic-nix-check-password
system=$(nix eval --impure --raw --expr builtins.currentSystem)
case "$system" in *-linux) ;; *) echo 'This check needs native NixOS.' >&2; exit 1 ;; esac
getent passwd gafctl >/dev/null
for path in "$fixture" "$state"; do
    test ! -e "$path"
done
for service in "$unit" "$broker"; do
    test "$(systemctl show --property=LoadState --value "$service")" = not-found
done
for port in 19787 19883; do
    if ss -ltnH "sport = :$port" | grep -q .; then
        echo "Test port $port is already in use." >&2
        exit 1
    fi
done
artifact=$(nix build ".#checks.$system.service" --no-link --print-out-paths)
cleanup() {
    "$privilege" systemctl stop "$unit" "$broker" || true
    "$privilege" systemctl disable --runtime "$unit" || true
    "$privilege" systemctl reset-failed "$unit" "$broker" >/dev/null 2>&1 || true
    "$privilege" rm -rf "$fixture" "$state"
}
trap cleanup EXIT
"$privilege" install -d -m 755 "$fixture"
printf '%s\n' "$password" | "$privilege" tee "$fixture/password" >/dev/null
"$privilege" chmod 600 "$fixture/password"
"$privilege" "$(command -v mosquitto_passwd)" -b -c "$fixture/mqtt-users" "$username" "$password"
"$privilege" chown gafctl:gafctl "$fixture/mqtt-users"
printf 'listener 19883 127.0.0.1\nallow_anonymous false\npassword_file %s/mqtt-users\n' "$fixture" |
    "$privilege" tee "$fixture/mosquitto.conf" >/dev/null
"$privilege" chmod 644 "$fixture/mosquitto.conf"
"$privilege" systemd-run --unit "$broker" --property=User=gafctl --property=Group=gafctl \
    "$(command -v mosquitto)" -c "$fixture/mosquitto.conf"
"$privilege" systemctl link --runtime "$artifact/$unit"
"$privilege" systemctl start "$unit"
ready() {
    curl --fail --silent --retry 15 --retry-connrefused --retry-delay 1 http://127.0.0.1:19787/health
}
ready
test "$(curl --fail --silent http://127.0.0.1:19787/api/v2/devices)" = '{"devices":[]}'
test "$(systemctl show --property=User --value "$unit")" = gafctl
test "$(stat -c '%U:%a' "$state")" = gafctl:700
pid=$(systemctl show --property=MainPID --value "$unit")
test "$(ps -o user= -p "$pid" | tr -d ' ')" = gafctl
test "$("$privilege" cat "/run/credentials/$unit/mqtt-password")" = "$password"
test "$(mosquitto_sub -h 127.0.0.1 -p 19883 -u "$username" -P "$password" \
    -t 'gafctl/+/availability' -C 1 -W 15)" = online
identity=$("$privilege" sha256sum "$state/identities.json")
"$privilege" systemctl restart "$unit"
ready
test "$("$privilege" sha256sum "$state/identities.json")" = "$identity"
"$privilege" -u gafctl busctl --system call org.bluez / org.freedesktop.DBus.ObjectManager GetManagedObjects >/dev/null
printf '\nNixOS unit, MQTT credentials, HTTP, service user, state persistence, BlueZ access: passed\n'
