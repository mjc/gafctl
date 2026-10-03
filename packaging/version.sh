#!/bin/sh
set -eu
root=${1:-$(dirname "$0")/..}
version=$(sed -n '/^\[package\]$/,/^\[/ { s/^version = "\([^"]*\)"/\1/p; }' "$root/Cargo.toml")
if [ -z "$version" ]; then
    echo 'Missing Cargo package version.' >&2
    exit 1
fi
if ! component_version=$(jq -er '.version | select(type == "string" and length > 0)' "$root/custom_components/gafctl/manifest.json"); then
    echo 'Missing or invalid Home Assistant component version.' >&2
    exit 1
fi
app_version=$(sed -n 's/^version: "\([^"]*\)"/\1/p' "$root/home-assistant/config.yaml")
if [ "$component_version" != "$version" ] || [ "$app_version" != "$version" ]; then
    printf 'Version mismatch: Cargo=%s, Home Assistant component=%s, app=%s\n' \
        "$version" "$component_version" "$app_version" >&2
    exit 1
fi
printf '%s\n' "$version"
