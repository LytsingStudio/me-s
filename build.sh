#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR=${ME_BUILD_CACHE_DIR:-$ROOT_DIR/.build-cache}
BUILD_OFFLINE=1
case ${1:-} in
    --offline)
        shift
        ;;
    --online)
        BUILD_OFFLINE=0
        shift
        ;;
esac
if [[ $# -ne 0 ]]; then
    echo "usage: $0 [--offline|--online]" >&2
    exit 2
fi

if [[ -f "$ROOT_DIR/../toolchains/me-client-env.sh" ]]; then
    # shellcheck disable=SC1091
    source "$ROOT_DIR/../toolchains/me-client-env.sh"
fi
if [[ -f "$CACHE_DIR/host-env.sh" ]]; then
    # shellcheck disable=SC1090
    source "$CACHE_DIR/host-env.sh"
fi
export PATH="$CACHE_DIR/bin:$PATH"
export ME_BUILD_CACHE_DIR="$CACHE_DIR"
export ME_BUILD_OFFLINE="$BUILD_OFFLINE"

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "error: missing build dependency: $1" >&2
        echo "initialize the persistent host toolchain in $CACHE_DIR before building" >&2
        exit 1
    }
}

for command in bun cargo docker file git lipo makensis node pkgbuild pkgutil rustup shasum tar xcrun; do
    require_command "$command"
done
for command in clang-cl llvm-ar llvm-lib llvm-rc; do
    require_command "$command"
done
if [[ -z "${ME_7Z:-}" ]]; then
    if command -v 7zz >/dev/null 2>&1; then
        export ME_7Z="$(command -v 7zz)"
    elif command -v 7z >/dev/null 2>&1; then
        export ME_7Z="$(command -v 7z)"
    else
        echo "error: 7zz or 7z is required for static Windows package inspection" >&2
        exit 1
    fi
fi
[[ -x "$ME_7Z" ]] || { echo "error: ME_7Z is not executable: $ME_7Z" >&2; exit 1; }
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || {
    echo "error: the all-platform builder requires an Apple Silicon macOS host" >&2
    exit 1
}
cargo xwin --version >/dev/null 2>&1 || { echo "error: cargo-xwin is required" >&2; exit 1; }
cargo tauri --version >/dev/null 2>&1 || { echo "error: tauri-cli is required" >&2; exit 1; }

if ! docker info >/dev/null 2>&1; then
    if command -v colima >/dev/null 2>&1; then
        echo "starting persistent Colima build environment"
        colima start
    else
        echo "error: Docker is unavailable and Colima is not installed" >&2
        exit 1
    fi
fi

VERSION="$(node "$ROOT_DIR/scripts/product-version.cjs" --print)"
HEAD_COMMIT="$(git -C "$ROOT_DIR" rev-parse HEAD)"
SOURCE_DIRTY=false
if [[ -n "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=all)" ]]; then
    SOURCE_DIRTY=true
fi
WINDOWS_TARGET=x86_64-pc-windows-msvc
PACKAGE_ASSETS=(
    ME-macos-universal.pkg
    ME-windows-x86_64-setup.exe
    ME-linux-x86_64.run
    ME-linux-arm64.run
)

for target in aarch64-apple-darwin x86_64-apple-darwin "$WINDOWS_TARGET"; do
    rustup target list --installed | grep -Fx "$target" >/dev/null || {
        echo "error: missing Rust target $target; initialize it before offline builds" >&2
        exit 1
    }
done

mkdir -p "$ROOT_DIR/.build" "$CACHE_DIR/host" "$CACHE_DIR/xwin"
WORK=$(mktemp -d "$ROOT_DIR/.build/all-platform.XXXXXX")
STAGING_DIST="$WORK/dist"
BUILD_DIR="$WORK/package"
mkdir -p "$STAGING_DIST" "$BUILD_DIR/macos"

remove_transient_linux_containers() {
    local label containers
    for label in \
        studio.lytsing.me-s.build=linux-package \
        studio.lytsing.me-s.build=linux-environment
    do
        containers="$(docker ps --all --quiet --filter "label=$label")"
        if [[ -n "$containers" ]]; then
            docker rm --force $containers >/dev/null
        fi
    done
}

cleanup() {
    local status=$?
    trap - EXIT HUP INT TERM
    remove_transient_linux_containers || status=1
    rm -rf "$WORK"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

cd "$ROOT_DIR"
node scripts/product-version.cjs --print >/dev/null
sh -n install.sh
remove_transient_linux_containers

normalized_cargo_lock() {
    local package_name=$1
    local path=$2
    awk -v package_name="$package_name" '
        $0 == "name = \"" package_name "\"" { product_package = 1 }
        product_package && /^version = / {
            print "version = \"<product-version>\""
            product_package = 0
            next
        }
        { print }
    ' "$path"
}

normalized_package_json() {
    awk '
        /^[[:space:]]*"version"[[:space:]]*:/ {
            sub(/"version"[[:space:]]*:[[:space:]]*"[^"]*"/, "\"version\": \"<product-version>\"")
        }
        { print }
    ' "$1"
}

HOST_DEPENDENCY_FINGERPRINT="$(
    {
        normalized_cargo_lock me-s Cargo.lock
        normalized_cargo_lock me-client me-client/src-tauri/Cargo.lock
        normalized_package_json me-client/package.json
    } | shasum -a 256 | awk '{print $1}'
)"
HOST_DEPENDENCY_MARKER="$CACHE_DIR/host/dependencies-$HOST_DEPENDENCY_FINGERPRINT.ready"
HOST_OFFLINE=$BUILD_OFFLINE
if [[ $HOST_OFFLINE == 1 ]]; then
    [[ -f "$HOST_DEPENDENCY_MARKER" ]] || {
        echo "error: host dependencies have not completed their online initialization" >&2
        echo "run ./build.sh --online once to initialize the current dependency set" >&2
        exit 1
    }
fi
CARGO_OFFLINE_FLAG=
if [[ $HOST_OFFLINE == 1 ]]; then
    CARGO_OFFLINE_FLAG=--offline
    export CARGO_NET_OFFLINE=true
    echo "building host targets from persistent offline dependency caches"
else
    unset CARGO_NET_OFFLINE
    echo "building host targets and initializing persistent dependency caches"
fi
export RUSTFLAGS="--remap-path-prefix=$ROOT_DIR=/source"

echo "building macOS universal programs"
cargo build --locked ${CARGO_OFFLINE_FLAG:+$CARGO_OFFLINE_FLAG} --release --bins --target aarch64-apple-darwin
cargo build --locked ${CARGO_OFFLINE_FLAG:+$CARGO_OFFLINE_FLAG} --release --bins --target x86_64-apple-darwin
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
    cargo tauri build --target universal-apple-darwin --bundles app
)
MAC_APP="$(find me-client/src-tauri/target/universal-apple-darwin/release/bundle/macos -maxdepth 1 -name '*.app' -print -quit)"
[[ -n "$MAC_APP" ]] || { echo "error: macOS ME Client bundle was not created" >&2; exit 1; }
packaging/macos/build-pkg.sh \
    "$VERSION" \
    "$BUILD_DIR/macos/me-s" \
    "$BUILD_DIR/macos/me-gateway" \
    "$MAC_APP" \
    "$STAGING_DIST/ME-macos-universal.pkg"

echo "cross-compiling Windows x64 programs"
XWIN_CACHE_DIR="$CACHE_DIR/xwin" cargo xwin build \
    --locked ${CARGO_OFFLINE_FLAG:+$CARGO_OFFLINE_FLAG} --release --bins --target "$WINDOWS_TARGET"
(
    cd me-client
    bun run build
)
XWIN_CACHE_DIR="$CACHE_DIR/xwin" cargo xwin build \
    --locked ${CARGO_OFFLINE_FLAG:+$CARGO_OFFLINE_FLAG} --release --target "$WINDOWS_TARGET" \
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
    "$STAGING_DIST/ME-windows-x86_64-setup.exe"

echo "building Linux packages with persistent local toolchains and caches"
packaging/linux/build-container.sh "$VERSION" x86_64 "$STAGING_DIST/ME-linux-x86_64.run"
packaging/linux/build-container.sh "$VERSION" arm64 "$STAGING_DIST/ME-linux-arm64.run"

(
    cd "$STAGING_DIST"
    shasum -a 256 "${PACKAGE_ASSETS[@]}" >SHA256SUMS
)
node scripts/build-manifest.cjs create "$STAGING_DIST" "$VERSION" "$HEAD_COMMIT" "$SOURCE_DIRTY"
scripts/verify-release-artifacts.sh "$STAGING_DIST"
node scripts/build-manifest.cjs verify "$STAGING_DIST" "$VERSION" "$HEAD_COMMIT" any

touch "$HOST_DEPENDENCY_MARKER"
PREVIOUS_DIST="$ROOT_DIR/.build/dist.previous.$$"
rm -rf "$PREVIOUS_DIST"
if [[ -e "$ROOT_DIR/dist" ]]; then
    mv "$ROOT_DIR/dist" "$PREVIOUS_DIST"
fi
if mv "$STAGING_DIST" "$ROOT_DIR/dist"; then
    rm -rf "$PREVIOUS_DIST"
else
    [[ ! -e "$PREVIOUS_DIST" ]] || mv "$PREVIOUS_DIST" "$ROOT_DIR/dist"
    exit 1
fi

if command -v colima >/dev/null 2>&1 && [[ "$(docker context show)" == colima ]]; then
    echo "reclaiming blocks released by transient Linux build containers"
    (cd / && colima ssh -- sudo fstrim -v /var/lib/docker)
fi

echo "all platform assets were built and statically verified in $ROOT_DIR/dist"
