#!/usr/bin/env bash
set -euo pipefail
image=${1:?Pass the destination image}
version=${2:?Pass the release version}
artifacts=${3:?Pass the downloaded image artifact directory}
packaging/publish-github-release.sh --check
packaging/publish-images.sh "$image" "$version" "$artifacts" --check
packaging/publish-images.sh "$image" "$version" "$artifacts"
packaging/publish-github-release.sh
