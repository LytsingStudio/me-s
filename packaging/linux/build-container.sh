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
        RUST_TARGET=x86_64-unknown-linux-gnu
        ASSET_NAME=ME-linux-x86_64.run
        ;;
    arm64)
        PLATFORM=linux/arm64
        RUST_TARGET=aarch64-unknown-linux-gnu
        ASSET_NAME=ME-linux-arm64.run
        ;;
    *)
        echo "error: unsupported Linux architecture: $ARCH" >&2
        exit 1
        ;;
esac

command -v docker >/dev/null 2>&1 || { echo "error: Docker is required" >&2; exit 1; }
docker buildx version >/dev/null 2>&1 || { echo "error: Docker Buildx is required" >&2; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/me-linux-container.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM
mkdir -p "$WORK/output" "$(dirname "$OUTPUT")"

docker buildx build \
    --platform "$PLATFORM" \
    --build-arg "ME_VERSION=$VERSION" \
    --build-arg "RUST_TARGET=$RUST_TARGET" \
    --build-arg "PACKAGE_ARCH=$ARCH" \
    --build-arg "ASSET_NAME=$ASSET_NAME" \
    --output "type=local,dest=$WORK/output" \
    --progress plain \
    --file "$ROOT_DIR/packaging/linux/Dockerfile" \
    "$ROOT_DIR"

PACKAGE="$WORK/output/$ASSET_NAME"
[[ -s "$PACKAGE" ]] || { echo "error: Linux container build did not create $ASSET_NAME" >&2; exit 1; }
install -m 755 "$PACKAGE" "$OUTPUT"
echo "built ME Linux $ARCH package: $OUTPUT"
