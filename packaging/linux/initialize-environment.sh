#!/usr/bin/env bash
set -euo pipefail

: "${RUST_TARGET:?RUST_TARGET is required}"
: "${TARGETARCH:?TARGETARCH is required}"
: "${RUST_VERSION:?RUST_VERSION is required}"
: "${ZIG_VERSION:?ZIG_VERSION is required}"
: "${BUN_VERSION:?BUN_VERSION is required}"
: "${CARGO_ZIGBUILD_VERSION:?CARGO_ZIGBUILD_VERSION is required}"
: "${TAURI_CLI_VERSION:?TAURI_CLI_VERSION is required}"

export DEBIAN_FRONTEND=noninteractive
export CARGO_HOME=/root/.cargo
export RUSTUP_HOME=/root/.rustup
export PATH="/opt/bun/bin:/root/.cargo/bin:/opt/zig:$PATH"

apt-get update
apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl file libayatana-appindicator3-dev libfuse2 \
    librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev libxdo-dev patchelf pkg-config \
    squashfs-tools tar unzip xz-utils
rm -rf /var/lib/apt/lists/*

case "$TARGETARCH" in
    amd64)
        zig_arch=x86_64
        bun_arch=x64
        ;;
    arm64)
        zig_arch=aarch64
        bun_arch=aarch64
        ;;
    *)
        printf 'unsupported Docker target architecture: %s\n' "$TARGETARCH" >&2
        exit 1
        ;;
esac

curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz
mkdir -p /opt/zig
tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig
rm /tmp/zig.tar.xz

curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | \
    sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"
rustup target add "$RUST_TARGET"
cargo install cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" --locked
cargo install tauri-cli --version "$TAURI_CLI_VERSION" --locked

curl -fsSL \
    "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-${bun_arch}.zip" \
    -o /tmp/bun.zip
mkdir -p /tmp/bun /opt/bun/bin
unzip -q /tmp/bun.zip -d /tmp/bun
install -m 755 "/tmp/bun/bun-linux-${bun_arch}/bun" /opt/bun/bin/bun
ln -sf bun /opt/bun/bin/bunx
rm -rf /tmp/bun /tmp/bun.zip

bash /usr/local/bin/me-prepare-appimage-tools

rustc --version
cargo zigbuild --help >/dev/null
cargo tauri --version
bun --version
zig version
