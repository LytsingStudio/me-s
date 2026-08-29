#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_ONLY=false
if [[ ${1:-} == --build-only ]]; then
    BUILD_ONLY=true
    shift
fi
if [[ $# -ne 0 ]]; then
    echo "usage: $0 [--build-only]" >&2
    exit 2
fi

VERSION="$(node "$ROOT_DIR/scripts/product-version.cjs" --print)"
TAG="v$VERSION"
EXPECTED_REPOSITORY="${ME_RELEASE_REPOSITORY:-LytsingStudio/me-s}"
EXPECTED_BRANCH="${ME_RELEASE_BRANCH:-s}"
DIST_DIR="$ROOT_DIR/dist"
BUILD_DIR="$ROOT_DIR/.build/release"
CACHE_DIR="${ME_RELEASE_CACHE_DIR:-$ROOT_DIR/.build/release-cache}"
WINDOWS_TARGET=x86_64-pc-windows-msvc
PACKAGE_ASSETS=(
    ME-macos-universal.pkg
    ME-windows-x86_64-setup.exe
    ME-linux-x86_64.run
    ME-linux-arm64.run
)

require_command() {
    command -v "$1" >/dev/null 2>&1 || { echo "error: missing release dependency: $1" >&2; exit 1; }
}

for command in bun cargo docker file git lipo makensis node pkgbuild pkgutil rustup shasum xcrun; do
    require_command "$command"
done
cargo xwin --version >/dev/null 2>&1 || { echo "error: cargo-xwin is required" >&2; exit 1; }
docker buildx version >/dev/null 2>&1 || { echo "error: Docker Buildx is required" >&2; exit 1; }
for command in clang-cl llvm-ar llvm-lib llvm-rc; do
    require_command "$command"
done
if [[ -z "${ME_7Z:-}" ]]; then
    if ! command -v 7zz >/dev/null 2>&1 && ! command -v 7z >/dev/null 2>&1; then
        echo "error: 7zz or 7z is required for static NSIS inspection" >&2
        exit 1
    fi
elif [[ ! -x "$ME_7Z" ]]; then
    echo "error: ME_7Z is not executable: $ME_7Z" >&2
    exit 1
fi
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || {
    echo "error: the local release builder requires an Apple Silicon macOS host" >&2
    exit 1
}
rustup target list --installed | grep -Fx aarch64-apple-darwin >/dev/null
rustup target list --installed | grep -Fx x86_64-apple-darwin >/dev/null
rustup target list --installed | grep -Fx "$WINDOWS_TARGET" >/dev/null

cd "$ROOT_DIR"
node scripts/product-version.cjs --print >/dev/null
sh -n install.sh

HEAD_COMMIT="$(git rev-parse HEAD)"
REPOSITORY=""
if ! $BUILD_ONLY; then
    require_command gh
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
fi

rm -rf "$DIST_DIR" "$BUILD_DIR"
mkdir -p "$DIST_DIR" "$BUILD_DIR/macos" "$CACHE_DIR/xwin"
export RUSTFLAGS="--remap-path-prefix=$ROOT_DIR=/source"

echo "building macOS universal programs"
cargo build --locked --release --bins --target aarch64-apple-darwin
cargo build --locked --release --bins --target x86_64-apple-darwin
lipo -create \
    target/aarch64-apple-darwin/release/me-s \
    target/x86_64-apple-darwin/release/me-s \
    -output "$BUILD_DIR/macos/me-s"
lipo -create \
    target/aarch64-apple-darwin/release/me-gateway \
    target/x86_64-apple-darwin/release/me-gateway \
    -output "$BUILD_DIR/macos/me-gateway"
(
    cd me-client
    bunx @tauri-apps/cli@2.11.3 build --target universal-apple-darwin --bundles app
)
MAC_APP="$(find me-client/src-tauri/target/universal-apple-darwin/release/bundle/macos -maxdepth 1 -name '*.app' -print -quit)"
[[ -n "$MAC_APP" ]] || { echo "error: macOS ME Client bundle was not created" >&2; exit 1; }
packaging/macos/build-pkg.sh \
    "$VERSION" \
    "$BUILD_DIR/macos/me-s" \
    "$BUILD_DIR/macos/me-gateway" \
    "$MAC_APP" \
    "$DIST_DIR/ME-macos-universal.pkg"

echo "cross-compiling Windows x64 programs"
XWIN_CACHE_DIR="$CACHE_DIR/xwin" cargo xwin build \
    --locked --release --bins --target "$WINDOWS_TARGET"
(
    cd me-client
    bun run build
)
XWIN_CACHE_DIR="$CACHE_DIR/xwin" cargo xwin build \
    --locked --release --target "$WINDOWS_TARGET" \
    --manifest-path me-client/src-tauri/Cargo.toml \
    --bin me-client
for binary in \
    "target/$WINDOWS_TARGET/release/me-s.exe" \
    "target/$WINDOWS_TARGET/release/me-gateway.exe" \
    "me-client/src-tauri/target/$WINDOWS_TARGET/release/me-client.exe"
do
    [[ -s "$binary" ]] || { echo "error: missing Windows program: $binary" >&2; exit 1; }
    file "$binary" | grep -E 'PE32\+ executable .* x86-64' >/dev/null
    xcrun llvm-objdump --file-headers "$binary" | grep -F 'file format coff-x86-64' >/dev/null
done
packaging/windows/build-installer.sh \
    "$VERSION" \
    "target/$WINDOWS_TARGET/release/me-s.exe" \
    "target/$WINDOWS_TARGET/release/me-gateway.exe" \
    "me-client/src-tauri/target/$WINDOWS_TARGET/release/me-client.exe" \
    "$DIST_DIR/ME-windows-x86_64-setup.exe"

echo "building Linux product packages in local containers"
packaging/linux/build-container.sh "$VERSION" x86_64 "$DIST_DIR/ME-linux-x86_64.run"
packaging/linux/build-container.sh "$VERSION" arm64 "$DIST_DIR/ME-linux-arm64.run"

(
    cd "$DIST_DIR"
    shasum -a 256 "${PACKAGE_ASSETS[@]}" >SHA256SUMS
)
scripts/verify-release-artifacts.sh "$DIST_DIR"

echo "all release assets were built and statically verified in $DIST_DIR"
if $BUILD_ONLY; then
    exit 0
fi

echo "publishing $TAG from $HEAD_COMMIT"
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
REMOTE_TAG_COMMIT="$(git ls-remote origin "refs/tags/$TAG^{}" | awk '{print $1}')"
if [[ "$REMOTE_TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
    echo "error: published tag does not point to the release commit" >&2
    exit 1
fi

echo "published release:"
gh release view "$TAG" --repo "$REPOSITORY" --json url --jq .url
