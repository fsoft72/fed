#!/usr/bin/env bash
# Publish a new version of fed: bump Cargo.toml (and Cargo.lock), commit, tag vX.Y.Z and
# push the branch and the tag to origin. The tag triggers the GitHub release workflow.
#
# Usage: scripts/publish.sh 0.1.2
set -euo pipefail

REMOTE="origin"
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "${PROJECT_ROOT}"

# Prints an error message and exits.
die() {
	echo "error: $*" >&2
	exit 1
}

[[ $# -eq 1 ]] || die "usage: $0 <version>   (e.g. $0 0.1.2)"

VERSION="${1#v}"
TAG="v${VERSION}"

[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
	|| die "invalid version '${VERSION}' (expected X.Y.Z)"

[[ -z "$(git status --porcelain)" ]] || die "working tree is not clean, commit or stash first"
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null && die "tag ${TAG} already exists locally"
git ls-remote --exit-code --tags "${REMOTE}" "refs/tags/${TAG}" >/dev/null 2>&1 \
	&& die "tag ${TAG} already exists on ${REMOTE}"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "${BRANCH}" != "HEAD" ]] || die "detached HEAD, check out a branch first"

CURRENT="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
[[ "${CURRENT}" != "${VERSION}" ]] || die "Cargo.toml is already at ${VERSION}"

echo "Releasing ${CURRENT} -> ${VERSION} (branch ${BRANCH})"

# Only the first `version` line is the package version; dependencies are untouched.
sed -i "0,/^version = \".*\"/s//version = \"${VERSION}\"/" Cargo.toml

# Refreshes the fed entry of Cargo.lock and checks that the project still builds.
cargo check --quiet --locked 2>/dev/null || cargo check --quiet

git add Cargo.toml Cargo.lock
git commit -m "release ${TAG}"
git tag -a "${TAG}" -m "fed ${VERSION}"

git push "${REMOTE}" "${BRANCH}"
git push "${REMOTE}" "${TAG}"

echo "Published ${TAG}: the release workflow is now building the binaries."
