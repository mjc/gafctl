#!/usr/bin/env bash
set -euo pipefail

find_release_id_including_drafts() {
    gh api --paginate --slurp "repos/$GITHUB_REPOSITORY/releases?per_page=100" | jq -r --arg tag "$RELEASE_TAG" '
        [.[][] | select(.tag_name == $tag)] |
        if length > 1 then error("multiple releases for one tag") else .[0].id // empty end
    '
}

check_release_assets() {
    gh api --paginate --slurp "repos/$GITHUB_REPOSITORY/releases/$release_id/assets?per_page=100" |
        jq -e --arg mode "$1" --args '
            [.[][] | .name] | sort as $actual |
            ($ARGS.positional | sort) as $expected |
            if $mode == "draft" then ($actual - $expected | length) == 0
            else $actual == $expected end
        ' "${assets[@]}" >/dev/null || {
            printf 'Release asset names do not match the expected files.\n' >&2
            return 1
        }
}

assets=(SHA256SUMS)
for arch in amd64 arm64; do
    assets+=("gafctl_${RELEASE_VERSION}_${arch}.deb" "gafctl_${RELEASE_VERSION}_linux_${arch}.tar.gz" "provenance-$arch.json")
done
paths=("${assets[@]/#/dist/}")
for path in "${paths[@]}"; do test -s "$path"; done
(cd dist && sha256sum --check SHA256SUMS)
notes="docs/releases/$RELEASE_VERSION.md"
test -s "$notes"

release_id=$(find_release_id_including_drafts)
if [[ -z $release_id ]]; then
    gh release create "$RELEASE_TAG" "${paths[@]}" --verify-tag --title "gafctl $RELEASE_VERSION" --notes-file "$notes"
    exit 0
fi
state=$(gh api "repos/$GITHUB_REPOSITORY/releases/$release_id")
release_state=$(jq -er '
    if .draft == true then "draft"
    elif .draft == false then "published"
    else error("missing release draft status")
    end
' <<< "$state")

check_release_assets "$release_state"
if [[ $release_state == published ]]; then
    mkdir -p target/github-release
    downloaded=$(mktemp -d "$PWD/target/github-release/run.XXXXXX")
    trap 'rm -rf "$downloaded"' EXIT
    for asset in "${assets[@]}"; do
        gh release download "$RELEASE_TAG" --pattern "$asset" --dir "$downloaded"
        cmp "$downloaded/$asset" "dist/$asset"
    done
    printf 'GitHub release already published with identical assets.\n'
else
    gh release upload "$RELEASE_TAG" "${paths[@]}" --clobber
    check_release_assets published
    gh release edit "$RELEASE_TAG" --draft=false --notes-file "$notes"
fi
