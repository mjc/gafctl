#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(packaging/version.sh)
work="$PWD/target/crates-publication"
mkdir -p "$work"
cargo package --workspace --locked
cargo metadata --locked --no-deps --format-version 1 > "$work/metadata.json"
jq -e --arg version "$version" '
    .workspace_members as $members |
    all(.packages[] | select(.id as $id | $members | index($id));
        .version == $version and .publish == ["crates-io"])
' "$work/metadata.json" >/dev/null
mapfile -t packages < <(jq -r '
    .workspace_members as $members |
    .packages[] | select(.id as $id | $members | index($id)) | .name
' "$work/metadata.json")
missing=()
for package in "${packages[@]}"; do
    archive="target/package/$package-$version.crate"
    status=$(curl --silent --show-error --location --retry 5 \
        --user-agent 'gafctl-release (https://github.com/mjc/gafctl)' \
        --output "$work/$package.json" --write-out '%{http_code}' \
        "https://crates.io/api/v1/crates/$package/$version")
    case "$status" in
        200)
            checksum=$(jq -er --arg name "$package" --arg version "$version" '
                .version | select(.crate == $name and .num == $version and .yanked == false) | .checksum
            ' "$work/$package.json")
            printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check
            ;;
        404) missing+=(--package "$package") ;;
        *) printf 'Cannot check %s %s: HTTP %s\n' "$package" "$version" "$status" >&2; exit 1 ;;
    esac
done
if (( ${#missing[@]} > 0 )); then
    cargo publish --locked --registry crates-io "${missing[@]}"
fi
for package in "${packages[@]}"; do
    curl --fail --silent --show-error --location --retry 10 --retry-all-errors --retry-delay 3 \
        --user-agent 'gafctl-release (https://github.com/mjc/gafctl)' \
        --output "$work/$package.crate" \
        "https://static.crates.io/crates/$package/$package-$version.crate"
    cmp "target/package/$package-$version.crate" "$work/$package.crate"
done
