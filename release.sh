#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -ne 0 ]]; then
    echo "usage: $0" >&2
    exit 2
fi

VERSION="$(node "$ROOT_DIR/scripts/product-version.cjs" --print)"
TAG="v$VERSION"
EXPECTED_REPOSITORY="${ME_RELEASE_REPOSITORY:-LytsingStudio/me-s}"
EXPECTED_BRANCH="${ME_RELEASE_BRANCH:-s}"
DIST_DIR="$ROOT_DIR/dist"
PACKAGE_ASSETS=(
    ME-macos-universal.pkg
    ME-windows-x86_64-setup.exe
    ME-linux-x86_64.run
    ME-linux-arm64.run
)

require_command() {
    command -v "$1" >/dev/null 2>&1 || { echo "error: missing release dependency: $1" >&2; exit 1; }
}
for command in file gh git lipo node pkgutil shasum tar xcrun; do
    require_command "$command"
done
if [[ -z "${ME_7Z:-}" ]]; then
    if command -v 7zz >/dev/null 2>&1; then
        export ME_7Z="$(command -v 7zz)"
    elif command -v 7z >/dev/null 2>&1; then
        export ME_7Z="$(command -v 7z)"
    elif [[ -x "$ROOT_DIR/.build-cache/bin/7zz" ]]; then
        export ME_7Z="$ROOT_DIR/.build-cache/bin/7zz"
    else
        echo "error: 7zz or 7z is required for static Windows package inspection" >&2
        exit 1
    fi
fi
[[ -x "$ME_7Z" ]] || { echo "error: ME_7Z is not executable: $ME_7Z" >&2; exit 1; }

cd "$ROOT_DIR"
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    echo "error: release requires a clean Git worktree" >&2
    exit 1
fi
BRANCH="$(git branch --show-current)"
if [[ "$BRANCH" != "$EXPECTED_BRANCH" ]]; then
    echo "error: release must run from $EXPECTED_BRANCH; current branch is ${BRANCH:-detached HEAD}" >&2
    exit 1
fi
if ! gh auth status --hostname github.com >/dev/null 2>&1; then
    echo "error: GitHub CLI is not logged in; run gh auth login" >&2
    exit 1
fi
REPOSITORY="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
if [[ "$REPOSITORY" != "$EXPECTED_REPOSITORY" ]]; then
    echo "error: release repository must be $EXPECTED_REPOSITORY; current origin resolves to $REPOSITORY" >&2
    exit 1
fi

git fetch --quiet origin "$BRANCH" --tags
HEAD_COMMIT="$(git rev-parse HEAD)"
if [[ "$HEAD_COMMIT" != "$(git rev-parse "origin/$BRANCH")" ]]; then
    echo "error: local HEAD is not the commit currently pushed to origin/$BRANCH" >&2
    exit 1
fi
if git rev-parse --quiet --verify "refs/tags/$TAG" >/dev/null; then
    echo "error: tag $TAG already exists" >&2
    exit 1
fi
if gh release view "$TAG" --repo "$REPOSITORY" >/dev/null 2>&1; then
    echo "error: GitHub Release $TAG already exists in $REPOSITORY" >&2
    exit 1
fi

node scripts/build-manifest.cjs verify "$DIST_DIR" "$VERSION" "$HEAD_COMMIT" false
scripts/verify-release-artifacts.sh "$DIST_DIR"

echo "publishing prebuilt $TAG assets from $HEAD_COMMIT"
git tag -a "$TAG" -m "$TAG"
git push origin "refs/tags/$TAG"
gh release create "$TAG" \
    "$DIST_DIR/ME-macos-universal.pkg" \
    "$DIST_DIR/ME-windows-x86_64-setup.exe" \
    "$DIST_DIR/ME-linux-x86_64.run" \
    "$DIST_DIR/ME-linux-arm64.run" \
    "$DIST_DIR/SHA256SUMS#SHA-256 checksums" \
    --repo "$REPOSITORY" \
    --verify-tag \
    --title "$TAG" \
    --generate-notes \
    --latest

EXPECTED_ASSETS=$'ME-linux-arm64.run\nME-linux-x86_64.run\nME-macos-universal.pkg\nME-windows-x86_64-setup.exe\nSHA256SUMS'
ACTUAL_ASSETS="$(gh release view "$TAG" --repo "$REPOSITORY" --json assets --jq '.assets[].name' | LC_ALL=C sort)"
if [[ "$ACTUAL_ASSETS" != "$EXPECTED_ASSETS" ]]; then
    printf 'error: published Release asset set is invalid\nexpected:\n%s\nactual:\n%s\n' "$EXPECTED_ASSETS" "$ACTUAL_ASSETS" >&2
    exit 1
fi
if gh release view "$TAG" --repo "$REPOSITORY" --json assets --jq '.assets[] | select(.size <= 0) | .name' | grep . >/dev/null; then
    echo "error: published Release contains an empty asset" >&2
    exit 1
fi
REMOTE_TAG_COMMIT="$(git ls-remote origin "refs/tags/$TAG^{}" | awk '{print $1}')"
if [[ "$REMOTE_TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
    echo "error: published tag does not point to the release commit" >&2
    exit 1
fi

RELEASE_URL="$(gh release view "$TAG" --repo "$REPOSITORY" --json url --jq .url)"
echo "published release:"
printf '%s\n' "$RELEASE_URL"
