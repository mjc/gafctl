#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
work="$PWD/target/install-checks"
mkdir -p "$work/artifacts" "$work/data"
chmod 700 "$work/data"
if [ "$(uname -s)" = Linux ] && [ "${CONTAINER_ENGINE:-podman}" = podman ]; then
    session_runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
    export XDG_RUNTIME_DIR="$work/runtime"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    for node in bus systemd; do
        if [ -e "$session_runtime/$node" ] && [ ! -e "$XDG_RUNTIME_DIR/$node" ]; then
            ln -s "$session_runtime/$node" "$XDG_RUNTIME_DIR/$node"
        fi
    done
fi
engine() {
    if [ "${CONTAINER_ENGINE:-podman}" = docker ]; then
        docker "$@"
    elif [ "$(uname -s)" = Linux ]; then
        podman --root "$work/storage" --runroot "$work/runtime/storage" "$@"
    else
        podman "$@"
    fi
}
build() {
    if [ "${CONTAINER_ENGINE:-podman}" = docker ]; then
        engine build "$@"
    else
        engine build --format docker "$@"
    fi
}
cleanup() {
    result=$?
    for name in gafctl-check-export gafctl-check-runtime gafctl-check-app gafctl-check-package gafctl-check-fixtures; do
        if engine container inspect "$name" >/dev/null 2>&1; then
            if [ "$result" -ne 0 ]; then engine logs "$name"; fi
            engine rm --force --volumes "$name" >/dev/null
        fi
    done
    engine volume rm gafctl-check-fixtures gafctl-check-data >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM
if [ "${CONTAINER_ENGINE:-podman}" = docker ]; then
    arch=$(engine version --format '{{.Server.Arch}}')
else
    arch=$(engine info --format '{{.Host.Arch}}')
fi
case "$arch" in
    amd64) app_arch=amd64 ;;
    arm64) app_arch=aarch64 ;;
    *) echo "Unsupported runtime architecture: $arch" >&2; exit 1 ;;
esac
version=$(packaging/version.sh)
printf 'Checking Linux %s installation paths\n' "$arch"
build --target packages -t localhost/gafctl-install-packages:check .
build --target runtime -t gafctl:local .
build --build-arg "BUILD_VERSION=$version" --build-arg "BUILD_ARCH=$app_arch" -t localhost/gafctl-install-app:check home-assistant
engine create --name gafctl-check-export localhost/gafctl-install-packages:check >/dev/null
engine cp gafctl-check-export:/src/dist/. "$work/artifacts/"
engine rm gafctl-check-export >/dev/null
engine run -d --name gafctl-check-fixtures --entrypoint sleep \
    --mount type=volume,src=gafctl-check-fixtures,dst=/checks \
    --mount type=volume,src=gafctl-check-data,dst=/data gafctl:local infinity >/dev/null
engine exec gafctl-check-fixtures mkdir /checks/artifacts
engine exec gafctl-check-fixtures chmod 700 /data
engine cp "$work/artifacts/." gafctl-check-fixtures:/checks/artifacts/
engine cp packaging/check-package.sh gafctl-check-fixtures:/checks/check-package.sh
for base in ubuntu:24.04 debian:bookworm-slim; do
    build --build-arg "BASE_IMAGE=docker.io/library/$base" -f packaging/check-systemd.Dockerfile -t localhost/gafctl-check-systemd:check .
    systemd_args='--systemd always'
    if [ "${CONTAINER_ENGINE:-podman}" = docker ]; then
        systemd_args='--tmpfs /run --tmpfs /run/lock'
    fi
    # shellcheck disable=SC2086
    engine run -d --privileged $systemd_args --name gafctl-check-package \
        --volumes-from gafctl-check-fixtures:ro \
        localhost/gafctl-check-systemd:check >/dev/null
    if ! engine exec gafctl-check-package sh /checks/check-package.sh "$arch" "$version" /checks/artifacts; then
        engine exec gafctl-check-package journalctl -u gafctl.service --no-pager || true
        exit 1
    fi
    engine stop --time 10 gafctl-check-package >/dev/null
    engine rm gafctl-check-package >/dev/null
    printf '%s package and unit: passed\n' "$base"
done
for image in gafctl:local localhost/gafctl-install-app:check; do
    printf '%s\n' '{"device_id":"","quickconnect_username":"","quickconnect_password":"","quickconnect_role":"consumer","quickconnect_writes_enabled":false,"mqtt_host":"","mqtt_port":1883,"mqtt_username":"","mqtt_password":"","mqtt_discovery":false}' > "$work/data/options.json"
    engine cp "$work/data/options.json" gafctl-check-fixtures:/data/options.json
    engine run --init -d --name gafctl-check-runtime --read-only --cap-drop ALL --security-opt no-new-privileges \
        --mount type=volume,src=gafctl-check-data,dst=/data "$image" >/dev/null
    engine exec gafctl-check-runtime curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
    engine exec gafctl-check-runtime curl --fail --silent http://127.0.0.1:8787/api/v2/devices
    engine exec gafctl-check-runtime gafctl server --help >/dev/null
    engine stop --time 3 gafctl-check-runtime >/dev/null
    test "$(engine inspect --format '{{.State.ExitCode}}' gafctl-check-runtime)" = 0
    engine start gafctl-check-runtime >/dev/null
    engine exec gafctl-check-runtime curl --fail --silent --retry 5 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
    engine stop --time 3 gafctl-check-runtime >/dev/null
    test "$(engine inspect --format '{{.State.ExitCode}}' gafctl-check-runtime)" = 0
    engine rm gafctl-check-runtime >/dev/null
    printf '\n%s default options, HTTP, restart, shutdown: passed\n' "$image"
done
printf '%s\n' '{"quickconnect_username":"packaging-check","quickconnect_password":"false","quickconnect_role":"consumer","quickconnect_writes_enabled":false,"mqtt_discovery":false}' > "$work/data/options.json"
engine cp "$work/data/options.json" gafctl-check-fixtures:/data/options.json
engine run --rm -i --read-only --cap-drop ALL --mount type=volume,src=gafctl-check-data,dst=/data gafctl:local sh -es <<'CHECK'
    test "$GAFCTL_QUICKCONNECT_USERNAME" = packaging-check
    test "$(cat "$GAFCTL_QUICKCONNECT_PASSWORD_FILE")" = false
    test "$(stat -c %a "$GAFCTL_QUICKCONNECT_PASSWORD_FILE")" = 600
    test -z "${GAFCTL_QUICKCONNECT_WRITES_ENABLED-}"
    test -z "${GAFCTL_MQTT_DISCOVERY-}"
CHECK
printf 'option mapping, disabled flags, string passwords, private files: passed\n'
GAFCTL_DEVICE_ID=packaging-check docker-compose -f compose.yaml -f compose.bluetooth.yaml -f compose.quickconnect.yaml config --quiet
actionlint .github/workflows/*.yml
shellcheck packaging/*.sh packaging/debian/* home-assistant/run.sh
printf 'Compose configuration, workflow, shell scripts: passed\n'
