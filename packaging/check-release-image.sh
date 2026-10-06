#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
image=${1:?Pass the loaded image tag}
arch=${2:?Pass the expected native architecture}
revision=${3:?Pass the expected source revision}
version=$(packaging/version.sh)
test "$(docker image inspect --format '{{.Architecture}}' "$image")" = "$arch"
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$image")" = "$revision"
container=$(docker run --detach --mount type=volume,dst=/data --read-only --cap-drop ALL --security-opt no-new-privileges "$image")
cleanup() {
    result=$?
    if [ "$result" -ne 0 ]; then docker logs "$container"; fi
    docker rm --force --volumes "$container" >/dev/null
}
trap cleanup EXIT HUP INT TERM
for executable in gafctl gafctl-server; do
    test "$(docker exec "$container" "$executable" --version)" = "$executable $version"
done
for name in LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html; do
    docker exec "$container" cat "/usr/share/licenses/gafctl/$name" | cmp "$name" -
done
docker exec "$container" curl --fail --silent --retry 15 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
inventory=$(docker exec "$container" curl --fail --silent http://127.0.0.1:8787/api/v2/devices)
test "$inventory" = '{"devices":[]}'
identity=$(docker exec "$container" sha256sum /data/identities.json)
docker stop --time 5 "$container" >/dev/null
test "$(docker inspect --format '{{.State.ExitCode}}' "$container")" = 0
docker start "$container" >/dev/null
docker exec "$container" curl --fail --silent --retry 15 --retry-connrefused --retry-delay 1 http://127.0.0.1:8787/health
test "$(docker exec "$container" sha256sum /data/identities.json)" = "$identity"
printf '\nExact release image, HTTP, shutdown and identity persistence: passed\n'
