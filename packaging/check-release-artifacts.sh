#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
arch=${1:?Pass the package architecture}
version=$(packaging/version.sh)
mkdir -p target
work=$(mktemp -d "$PWD/target/release-artifacts.XXXXXX")
container=
cleanup() {
    result=$?
    if [ -n "$container" ]; then
        if [ "$result" -ne 0 ]; then docker logs "$container"; fi
        docker rm --force "$container" >/dev/null
    fi
    rm -rf "$work"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$work/bin"
tar -xzf "dist/gafctl_${version}_linux_${arch}.tar.gz" -C "$work/bin"
for name in LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt; do
    cmp "$name" "$work/bin/$name"
done
for executable in gafctl gafctl-server; do
    test "$("$work/bin/$executable" --version)" = "$executable $version"
done
python3 packaging/check-native.py --package "$work"
package="$PWD/dist/gafctl_${version}_${arch}.deb"
test "$(dpkg-deb -f "$package" Architecture)" = "$arch"
test "$(dpkg-deb -f "$package" Version)" = "$version"
sudo apt-get update
sudo apt-get install -y --no-install-recommends "$package"
for name in LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt; do
    cmp "$name" "/usr/share/doc/gafctl/$name"
done
for executable in gafctl gafctl-server; do
    test "$("/usr/bin/$executable" --version)" = "$executable $version"
done
python3 packaging/check-native.py --package /usr
for base in ubuntu:24.04 debian:bookworm-slim; do
    docker build --build-arg "BASE_IMAGE=docker.io/library/$base" \
        -f packaging/check-systemd.Dockerfile -t gafctl-package-check:local .
    container=$(docker run --detach --privileged --tmpfs /run --tmpfs /run/lock \
        --mount "type=bind,src=$PWD/dist,dst=/artifacts,readonly" gafctl-package-check:local)
    docker cp packaging/check-package.sh "$container:/check-package.sh"
    docker exec "$container" sh /check-package.sh "$arch" "$version"
    docker stop --time 10 "$container" >/dev/null
    docker rm "$container" >/dev/null
    container=
    printf '%s exact package and systemd lifecycle: passed\n' "$base"
done
printf 'Exact release archive and Debian package: passed\n'
