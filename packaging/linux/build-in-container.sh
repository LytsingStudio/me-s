#!/usr/bin/env bash
set -euo pipefail

: "${ME_VERSION:?ME_VERSION is required}"
: "${RUST_TARGET:?RUST_TARGET is required}"
: "${PACKAGE_ARCH:?PACKAGE_ARCH is required}"
: "${ASSET_NAME:?ASSET_NAME is required}"
: "${TARGETARCH:?TARGETARCH is required}"

SOURCE_ARCHIVE=${ME_SOURCE_ARCHIVE:-/input/source.tar}
SOURCE_DIR=${ME_SOURCE_DIR:-/source}
OUTPUT_DIR=${ME_OUTPUT_DIR:-/artifact}
ROOT_TARGET_DIR=${ME_ROOT_TARGET_DIR:-/cache/root-target}
CLIENT_TARGET_DIR=${ME_CLIENT_TARGET_DIR:-/cache/client-target}
PYTHON_CACHE_DIR=${ME_PYTHON_CACHE_DIR:-/cache/python}

export CARGO_HOME=${ME_CARGO_HOME:-/cache/cargo}
export RUSTUP_HOME=/root/.rustup
export PATH="/opt/me-linuxdeploy-plugin-appimage/usr/bin:/opt/bun/bin:/root/.cargo/bin:/opt/zig:$PATH"
export RUSTFLAGS=--remap-path-prefix=/source=/source
if [[ ${ME_BUILD_OFFLINE:-0} == 1 ]]; then
    export CARGO_NET_OFFLINE=true
fi

rm -rf "$SOURCE_DIR"
mkdir -p "$SOURCE_DIR" "$OUTPUT_DIR" "$CARGO_HOME" "$ROOT_TARGET_DIR" "$CLIENT_TARGET_DIR" "$PYTHON_CACHE_DIR"
tar -xf "$SOURCE_ARCHIVE" -C "$SOURCE_DIR"
mkdir -p "$SOURCE_DIR/.build"
rm -rf "$SOURCE_DIR/.build/python"
ln -s "$PYTHON_CACHE_DIR" "$SOURCE_DIR/.build/python"

cd "$SOURCE_DIR"
test "$(bun scripts/product-version.cjs --print)" = "$ME_VERSION"
CARGO_TARGET_DIR="$ROOT_TARGET_DIR" \
    cargo zigbuild --locked --release --bins --target "${RUST_TARGET}.2.17"
(
    cd me-client
    CARGO_TARGET_DIR="$CLIENT_TARGET_DIR" cargo tauri build --no-bundle
    CARGO_TARGET_DIR="$CLIENT_TARGET_DIR" cargo tauri bundle --verbose --bundles appimage
)

client=$(find "$CLIENT_TARGET_DIR/release/bundle/appimage" -maxdepth 1 -name '*.AppImage' -print -quit)
[[ -n "$client" ]]
packaging/linux/build-run.sh \
    "$ME_VERSION" \
    "$PACKAGE_ARCH" \
    "$ROOT_TARGET_DIR/$RUST_TARGET/release/me-s" \
    "$ROOT_TARGET_DIR/$RUST_TARGET/release/me-gateway" \
    "$client" \
    "$OUTPUT_DIR/$ASSET_NAME"
[[ -s "$OUTPUT_DIR/$ASSET_NAME" ]]
