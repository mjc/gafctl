#!/usr/bin/env bash
set -euo pipefail
image=${1:?Pass the destination image}
version=${2:?Pass the release version}
root=${3:?Pass the downloaded image artifact directory}
shopt -s nullglob dotglob
artifacts=("$root"/*)
if (( ${#artifacts[@]} == 0 )); then
    echo 'No image artifacts to publish.' >&2
    exit 1
fi
# Validate the complete set before pushing any image.
for artifact in "${artifacts[@]}"; do
    name=${artifact##*/}
    if [[ ! -d $artifact || -L $artifact || ! $name =~ ^image-[a-z0-9]+$ ]]; then
        printf 'Invalid image artifact directory: %s\n' "$artifact" >&2
        exit 1
    fi
    files=("$artifact"/*)
    if (( ${#files[@]} != 1 )) || [[ ${files[0]} != "$artifact/gafctl-image.tar" || ! -f ${files[0]} || -L ${files[0]} ]]; then
        printf 'Invalid image artifact contents: %s\n' "$artifact" >&2
        exit 1
    fi
done
members=()
for artifact in "${artifacts[@]}"; do
    arch=${artifact##*/image-}
    if docker image inspect gafctl:local >/dev/null 2>&1; then
        docker image rm --force gafctl:local >/dev/null
    fi
    docker load --input "$artifact/gafctl-image.tar"
    if ! actual_arch=$(docker image inspect --format '{{.Architecture}}' gafctl:local); then
        printf 'Image artifact did not load gafctl:local: %s\n' "$artifact" >&2
        exit 1
    fi
    if [[ $actual_arch != "$arch" ]]; then
        printf 'Image architecture mismatch: artifact=%s, image=%s\n' "$arch" "$actual_arch" >&2
        exit 1
    fi
    member="$image:$version-$arch"
    docker tag gafctl:local "$member"
    members+=("$member")
done
for member in "${members[@]}"; do
    docker push "$member"
done
docker manifest create "$image:$version" "${members[@]}"
docker manifest push "$image:$version"
