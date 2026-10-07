#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(packaging/version.sh)
work="$PWD/target/crates-publication"
mkdir -p "$work"
cargo package --locked
cargo metadata --locked --no-deps --format-version 1 > "$work/metadata.json"
jq -e --arg version "$version" '
    .packages | length == 1 and
    .[0].name == "gafctl" and .[0].version == $version and .[0].publish == ["crates-io"]
' "$work/metadata.json" >/dev/null
archive="target/package/gafctl-$version.crate"
status=$(curl --silent --show-error --location --retry 5 \
    --user-agent 'gafctl-release (https://github.com/mjc/gafctl)' \
    --output "$work/gafctl.json" --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/gafctl/$version")
case "$status" in
    200)
        checksum=$(jq -er --arg version "$version" '
            .version | select(.crate == "gafctl" and .num == $version and .yanked == false) | .checksum
        ' "$work/gafctl.json")
        printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check
        ;;
    404) cargo publish --locked --registry crates-io ;;
    *) printf 'Cannot check gafctl %s: HTTP %s\n' "$version" "$status" >&2; exit 1 ;;
esac
curl --fail --silent --show-error --location --retry 10 --retry-all-errors --retry-delay 3 \
    --user-agent 'gafctl-release (https://github.com/mjc/gafctl)' \
    --output "$work/gafctl.crate" \
    "https://static.crates.io/crates/gafctl/gafctl-$version.crate"
cmp "$archive" "$work/gafctl.crate"
