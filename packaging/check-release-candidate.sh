#!/usr/bin/env bash
set -euo pipefail

check_release_candidate() {
    local tag=${1:?Pass the release tag} tag_object commit version head_commit tag_commit
    local semver='(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)'
    if [[ ! $tag =~ ^v$semver$ ]]; then
        printf 'Expected a stable version tag such as v0.1.0.\n' >&2
        return 1
    fi
    if [[ -n $(git status --porcelain) ]]; then
        printf 'Release checkout must be clean.\n' >&2
        return 1
    fi
    git fetch --no-tags origin \
        'refs/heads/main:refs/remotes/origin/main' "refs/tags/$tag:refs/tags/$tag"
    tag_object=$(git rev-parse "refs/tags/$tag")
    if [[ $(git cat-file -t "$tag_object") != tag ]]; then
        printf 'Release tag must be annotated and signed.\n' >&2
        return 1
    fi
    if [[ -n ${EXPECTED_RELEASE_TAG_OBJECT:-} && $tag_object != "$EXPECTED_RELEASE_TAG_OBJECT" ]]; then
        printf 'Release tag object changed after candidate validation.\n' >&2
        return 1
    fi
    commit=$(gh api "repos/$GITHUB_REPOSITORY/git/tags/$tag_object" | jq -er --arg tag "$tag" '
        select(.verification.verified == true and .object.type == "commit" and .tag == $tag) | .object.sha
    ') || {
        printf 'GitHub must verify the release tag signature.\n' >&2
        return 1
    }
    head_commit=$(git rev-parse HEAD)
    tag_commit=$(git rev-parse "$tag^{commit}")
    if ! [[ $commit == "$head_commit" && $commit == "$tag_commit" ]]; then
        printf 'Release tag must point to the checked-out commit.\n' >&2
        return 1
    fi
    git merge-base --is-ancestor "$commit" origin/main || {
        printf 'Release commit must be on main.\n' >&2
        return 1
    }
    if [[ -n ${EXPECTED_RELEASE_COMMIT:-} && $commit != "$EXPECTED_RELEASE_COMMIT" ]]; then
        printf 'Release commit changed after candidate validation.\n' >&2
        return 1
    fi
    gh api "repos/$GITHUB_REPOSITORY/commits/$commit" | jq -e '.commit.verification.verified == true' >/dev/null || {
        printf 'GitHub must verify the release commit signature.\n' >&2
        return 1
    }
    version=$(packaging/version.sh)
    if [[ $tag != "v$version" ]]; then
        printf 'Release tag and application versions differ.\n' >&2
        return 1
    fi
    test -s "docs/releases/$version.md"
    printf 'commit=%s\nversion=%s\ntag_object=%s\n' "$commit" "$version" "$tag_object" >> "$GITHUB_OUTPUT"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    check_release_candidate "$@"
fi
