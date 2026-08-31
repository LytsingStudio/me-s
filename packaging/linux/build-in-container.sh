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
ZIG_VERSION=${ZIG_VERSION:-0.15.2}

export DEBIAN_FRONTEND=noninteractive
export CARGO_HOME=/root/.cargo
export RUSTUP_HOME=/root/.rustup
export BUN_INSTALL=/opt/bun
export PATH="/opt/me-linuxdeploy-plugin-appimage/usr/bin:/opt/bun/bin:/root/.cargo/bin:/opt/zig:$PATH"
export RUSTFLAGS=--remap-path-prefix=/source=/source

apt-get update
apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl file libayatana-appindicator3-dev libfuse2 \
    librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev libxdo-dev patchelf pkg-config \
    squashfs-tools tar unzip xz-utils
rm -rf /var/lib/apt/lists/*

case "$TARGETARCH" in
    amd64) zig_arch=x86_64 ;;
    arm64) zig_arch=aarch64 ;;
    *) printf 'unsupported Docker target architecture: %s\n' "$TARGETARCH" >&2; exit 1 ;;
esac
curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz
mkdir -p /opt/zig
tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig
rm /tmp/zig.tar.xz
zig version

curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | \
    sh -s -- -y --profile minimal --default-toolchain stable
rustup target add "$RUST_TARGET"
cargo install cargo-zigbuild --locked
curl --proto '=https' --tlsv1.2 -fsSL https://bun.sh/install | bash
bun --version

if [[ -f "$SOURCE_ARCHIVE" ]]; then
    rm -rf "$SOURCE_DIR"
    mkdir -p "$SOURCE_DIR"
    tar -xf "$SOURCE_ARCHIVE" -C "$SOURCE_DIR"
elif [[ ! -f "$SOURCE_DIR/Cargo.toml" ]]; then
    echo "error: Linux build source is unavailable" >&2
    exit 1
fi

cd "$SOURCE_DIR"
test "$(bun scripts/product-version.cjs --print)" = "$ME_VERSION"
cargo zigbuild --locked --release --bins --target "${RUST_TARGET}.2.17"
packaging/linux/prepare-amd64-appimage-tools.sh
(
    cd me-client
    bunx @tauri-apps/cli@2.11.3 build --no-bundle
    bunx @tauri-apps/cli@2.11.3 bundle --verbose --bundles appimage
)

client=$(find me-client/src-tauri/target/release/bundle/appimage -maxdepth 1 -name '*.AppImage' -print -quit)
[[ -n "$client" ]]
mkdir -p "$OUTPUT_DIR"
packaging/linux/build-run.sh \
    "$ME_VERSION" \
    "$PACKAGE_ARCH" \
    "target/$RUST_TARGET/release/me-s" \
    "target/$RUST_TARGET/release/me-gateway" \
    "$client" \
    "$OUTPUT_DIR/$ASSET_NAME"
[[ -s "$OUTPUT_DIR/$ASSET_NAME" ]]
