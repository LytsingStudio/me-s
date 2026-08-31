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

command -v docker >/dev/null 2>&1 || { echo "error: Docker is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "error: tar is required" >&2; exit 1; }

BASE_IMAGE=ubuntu:22.04
CONTAINER_NAME="me-s-linux-build-${ARCH//_/-}-$$"
BUILD_LABEL=studio.lytsing.me-s.release=linux-build
BASE_IMAGE_WAS_PRESENT=false
if docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
    BASE_IMAGE_WAS_PRESENT=true
fi

mkdir -p "$ROOT_DIR/.build"
WORK=$(mktemp -d "$ROOT_DIR/.build/linux-container.XXXXXX")
cleanup() {
    docker rm --force "$CONTAINER_NAME" >/dev/null 2>&1 || true
    if ! $BASE_IMAGE_WAS_PRESENT; then
        docker image rm "$BASE_IMAGE" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT HUP INT TERM
mkdir -p "$WORK/output" "$(dirname "$OUTPUT")"
COPYFILE_DISABLE=1 tar -cf "$WORK/source.tar" \
    --no-xattrs \
    --exclude-from="$ROOT_DIR/.dockerignore" \
    -C "$ROOT_DIR" \
    .

docker run --rm \
    --name "$CONTAINER_NAME" \
    --label "$BUILD_LABEL" \
    --platform "$PLATFORM" \
    --env "ME_VERSION=$VERSION" \
    --env "RUST_TARGET=$RUST_TARGET" \
    --env "PACKAGE_ARCH=$ARCH" \
    --env "ASSET_NAME=$ASSET_NAME" \
    --env "TARGETARCH=$TARGETARCH" \
    --volume "$WORK/source.tar:/input/source.tar:ro" \
    --volume "$WORK/output:/artifact" \
    --volume "$ROOT_DIR/packaging/linux/build-in-container.sh:/usr/local/bin/me-linux-build:ro" \
    "$BASE_IMAGE" \
    bash /usr/local/bin/me-linux-build

PACKAGE="$WORK/output/$ASSET_NAME"
[[ -s "$PACKAGE" ]] || { echo "error: Linux container build did not create $ASSET_NAME" >&2; exit 1; }
install -m 755 "$PACKAGE" "$OUTPUT"
echo "built ME Linux $ARCH package: $OUTPUT"
