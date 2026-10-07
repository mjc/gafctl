#!/usr/bin/env bash
set -euo pipefail
image=${1:?Pass the destination image}
version=${2:?Pass the release version}
root=${3:?Pass the downloaded image artifact directory}
mode=${4:-publish}
case "$mode" in publish|--check) ;; *) printf 'Usage: %s IMAGE VERSION DIRECTORY [--check]\n' "$0" >&2; exit 1 ;; esac
mkdir -p target/image-publication
work=$(mktemp -d "$PWD/target/image-publication/run.XXXXXX")
trap 'rm -rf "$work"' EXIT

inspect_registry() {
    local reference=$1 error
    if docker manifest inspect --verbose "$reference" > "$work/manifest.json" 2> "$work/registry-error"; then
        jq -e 'type == "object" or type == "array"' "$work/manifest.json" >/dev/null || return 1
        cat "$work/manifest.json"
    else
        error=$(cat "$work/registry-error")
        case "$error" in
            "no such manifest: $reference"|"manifest unknown"|"manifest unknown: manifest unknown"|"name unknown: repository name not known to registry")
                printf 'null\n' ;;
            *) printf '%s\n' "$error" >&2; return 1 ;;
        esac
    fi
}

image_identity() {
    jq -ce '
        {architecture: .Descriptor.platform.architecture,
         os: .Descriptor.platform.os,
         config: (.SchemaV2Manifest // .OCIManifest).config.digest} |
        if .os == "linux" and (.architecture | type == "string") and
           (.config | type == "string" and test("^sha256:[0-9a-f]{64}$"))
        then . else error("Invalid registry image identity") end
    '
}
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
missing=()
: > "$work/identities.jsonl"
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
    identity=$(docker image inspect --format '{{json .}}' "$member" |
        jq -ce '{architecture: .Architecture, os: .Os, config: .Id}')
    printf '%s\n' "$identity" >> "$work/identities.jsonl"
    manifest=$(inspect_registry "$member")
    if [[ $manifest == null ]]; then
        missing+=("$member")
    elif [[ $(image_identity <<< "$manifest") != "$identity" ]]; then
        printf 'Published image differs: %s\n' "$member" >&2
        exit 1
    fi
done

manifest=$(inspect_registry "$image:$version")
if [[ $manifest != null ]]; then
    expected=$(jq -sc 'sort_by(.architecture)' "$work/identities.jsonl")
    actual=$(jq -ce 'select(type == "array")' <<< "$manifest" |
        jq -c '.[]' | image_identity | jq -sc 'sort_by(.architecture)')
    if [[ $actual != "$expected" ]]; then
        printf 'Published multi-architecture image differs: %s:%s\n' "$image" "$version" >&2
        exit 1
    fi
fi
[[ $mode != --check ]] || exit 0
for member in "${missing[@]}"; do
    docker push "$member"
done
if [[ $manifest == null ]]; then
    docker manifest create "$image:$version" "${members[@]}"
    docker manifest push "$image:$version"
else
    printf 'Multi-architecture image already published with identical image identities.\n'
fi
