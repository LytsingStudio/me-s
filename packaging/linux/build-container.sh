#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <version> <x86_64|arm64> <output.run>" >&2
    exit 2
fi

VERSION=$1
ARCH=$2
OUTPUT=$3
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CACHE_DIR=${ME_BUILD_CACHE_DIR:-$ROOT_DIR/.build-cache}
BASE_IMAGE=ubuntu:22.04
RUST_VERSION=1.97.1
ZIG_VERSION=0.15.2
BUN_VERSION=1.3.14
CARGO_ZIGBUILD_VERSION=0.23.0
TAURI_CLI_VERSION=2.11.4
BUILD_LABEL=studio.lytsing.me-s.build=linux-package
INIT_LABEL=studio.lytsing.me-s.build=linux-environment

case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "error: invalid ME version: $VERSION" >&2; exit 1 ;;
esac

case "$ARCH" in
    x86_64)
        PLATFORM=linux/amd64
        TARGETARCH=amd64
        RUST_TARGET=x86_64-unknown-linux-gnu
        ASSET_NAME=ME-linux-x86_64.run
        ;;
    arm64)
        PLATFORM=linux/arm64
        TARGETARCH=arm64
        RUST_TARGET=aarch64-unknown-linux-gnu
        ASSET_NAME=ME-linux-arm64.run
        ;;
    *)
        echo "error: unsupported Linux architecture: $ARCH" >&2
        exit 1
        ;;
esac

for command in docker shasum tar; do
    command -v "$command" >/dev/null 2>&1 || { echo "error: $command is required" >&2; exit 1; }
done

ENVIRONMENT_FINGERPRINT="$(
    cat \
        "$ROOT_DIR/packaging/linux/initialize-environment.sh" \
        "$ROOT_DIR/packaging/linux/prepare-amd64-appimage-tools.sh" |
        shasum -a 256 | awk '{print substr($1, 1, 16)}'
)"
BUILDER_IMAGE="${ME_LINUX_BUILDER_IMAGE_PREFIX:-me-s-linux-builder}:${ENVIRONMENT_FINGERPRINT}-${TARGETARCH}"
INIT_CONTAINER="me-s-linux-init-${TARGETARCH}-$$"
BUILD_CONTAINER="me-s-linux-build-${TARGETARCH}-$$"
CARGO_VOLUME="me-s-linux-cargo-${TARGETARCH}-v1"
ROOT_TARGET_VOLUME="me-s-linux-root-target-${TARGETARCH}-v1"
CLIENT_TARGET_VOLUME="me-s-linux-client-target-${TARGETARCH}-v1"
PYTHON_VOLUME="me-s-linux-python-${TARGETARCH}-v1"

mkdir -p "$ROOT_DIR/.build" "$CACHE_DIR/linux/$TARGETARCH" "$(dirname "$OUTPUT")"
WORK=$(mktemp -d "$ROOT_DIR/.build/linux-container.XXXXXX")
cleanup() {
    docker rm --force "$INIT_CONTAINER" "$BUILD_CONTAINER" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT HUP INT TERM

ensure_builder_image() {
    if docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
        return
    fi
    if [[ ${ME_BUILD_OFFLINE:-1} == 1 ]]; then
        echo "error: offline Linux builder image is missing: $BUILDER_IMAGE" >&2
        echo "run ./build.sh --online once to initialize the current Linux environment" >&2
        exit 1
    fi
    echo "initializing persistent Linux $ARCH builder image $BUILDER_IMAGE"
    docker run \
        --name "$INIT_CONTAINER" \
        --label "$INIT_LABEL" \
        --platform "$PLATFORM" \
        --env "RUST_TARGET=$RUST_TARGET" \
        --env "TARGETARCH=$TARGETARCH" \
        --env "RUST_VERSION=$RUST_VERSION" \
        --env "ZIG_VERSION=$ZIG_VERSION" \
        --env "BUN_VERSION=$BUN_VERSION" \
        --env "CARGO_ZIGBUILD_VERSION=$CARGO_ZIGBUILD_VERSION" \
        --env "TAURI_CLI_VERSION=$TAURI_CLI_VERSION" \
        --volume "$ROOT_DIR/packaging/linux/initialize-environment.sh:/usr/local/bin/me-initialize-environment:ro" \
        --volume "$ROOT_DIR/packaging/linux/prepare-amd64-appimage-tools.sh:/usr/local/bin/me-prepare-appimage-tools:ro" \
        "$BASE_IMAGE" \
        bash /usr/local/bin/me-initialize-environment
    docker commit "$INIT_CONTAINER" "$BUILDER_IMAGE" >/dev/null
    docker rm "$INIT_CONTAINER" >/dev/null
    echo "initialized persistent Linux $ARCH builder image"
}

ensure_builder_image
for volume in "$CARGO_VOLUME" "$ROOT_TARGET_VOLUME" "$CLIENT_TARGET_VOLUME" "$PYTHON_VOLUME"; do
    if docker volume inspect "$volume" >/dev/null 2>&1; then
        continue
    fi
    if [[ ${ME_BUILD_OFFLINE:-1} == 1 ]]; then
        echo "error: offline Linux cache volume is missing: $volume" >&2
        echo "run ./build.sh --online once to initialize the current Linux caches" >&2
        exit 1
    fi
    docker volume create "$volume" >/dev/null
done

COPYFILE_DISABLE=1 tar -cf "$WORK/source.tar" \
    --no-xattrs \
    --exclude-from="$ROOT_DIR/.dockerignore" \
    -C "$ROOT_DIR" \
    .
mkdir -p "$WORK/output"

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

DEPENDENCY_FINGERPRINT="$(
    {
        normalized_cargo_lock me-s "$ROOT_DIR/Cargo.lock"
        normalized_cargo_lock me-client "$ROOT_DIR/me-client/src-tauri/Cargo.lock"
        cat "$ROOT_DIR/build.rs"
    } | shasum -a 256 | awk '{print $1}'
)"
DEPENDENCY_MARKER="$CACHE_DIR/linux/$TARGETARCH/dependencies-$DEPENDENCY_FINGERPRINT.ready"
OFFLINE=${ME_BUILD_OFFLINE:-1}
NETWORK_FLAG=
if [[ $OFFLINE == 1 ]]; then
    [[ -f "$DEPENDENCY_MARKER" ]] || {
        echo "error: Linux $ARCH dependencies have not completed their online initialization" >&2
        echo "run ./build.sh --online once to initialize the current dependency set" >&2
        exit 1
    }
    NETWORK_FLAG=--network=none
fi

if [[ $OFFLINE == 1 ]]; then
    echo "building Linux $ARCH from persistent offline toolchain and dependency caches"
else
    echo "building Linux $ARCH and initializing dependency caches"
fi

docker run --rm \
    --name "$BUILD_CONTAINER" \
    --label "$BUILD_LABEL" \
    --platform "$PLATFORM" \
    ${NETWORK_FLAG:+$NETWORK_FLAG} \
    --env "ME_VERSION=$VERSION" \
    --env "RUST_TARGET=$RUST_TARGET" \
    --env "PACKAGE_ARCH=$ARCH" \
    --env "ASSET_NAME=$ASSET_NAME" \
    --env "TARGETARCH=$TARGETARCH" \
    --env "ME_BUILD_OFFLINE=$OFFLINE" \
    --volume "$WORK/source.tar:/input/source.tar:ro" \
    --volume "$WORK/output:/artifact" \
    --volume "$ROOT_DIR/packaging/linux/build-in-container.sh:/usr/local/bin/me-linux-build:ro" \
    --volume "$CARGO_VOLUME:/cache/cargo" \
    --volume "$ROOT_TARGET_VOLUME:/cache/root-target" \
    --volume "$CLIENT_TARGET_VOLUME:/cache/client-target" \
    --volume "$PYTHON_VOLUME:/cache/python" \
    "$BUILDER_IMAGE" \
    bash /usr/local/bin/me-linux-build

PACKAGE="$WORK/output/$ASSET_NAME"
[[ -s "$PACKAGE" ]] || { echo "error: Linux container build did not create $ASSET_NAME" >&2; exit 1; }
touch "$DEPENDENCY_MARKER"
install -m 755 "$PACKAGE" "$OUTPUT"
echo "built ME Linux $ARCH package: $OUTPUT"
