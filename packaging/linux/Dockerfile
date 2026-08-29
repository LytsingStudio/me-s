# syntax=docker/dockerfile:1
FROM ubuntu:22.04 AS build

ARG TARGETARCH
ARG ME_VERSION
ARG RUST_TARGET
ARG PACKAGE_ARCH
ARG ASSET_NAME
ARG ZIG_VERSION=0.15.2

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    BUN_INSTALL=/opt/bun \
    PATH=/opt/me-linuxdeploy-plugin-appimage/usr/bin:/opt/bun/bin:/root/.cargo/bin:/opt/zig:${PATH} \
    RUSTFLAGS=--remap-path-prefix=/source=/source

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl file libayatana-appindicator3-dev libfuse2 \
        librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev libxdo-dev patchelf pkg-config \
        squashfs-tools tar unzip xz-utils && \
    rm -rf /var/lib/apt/lists/*

RUN case "$TARGETARCH" in \
        amd64) zig_arch=x86_64 ;; \
        arm64) zig_arch=aarch64 ;; \
        *) printf 'unsupported Docker target architecture: %s\n' "$TARGETARCH" >&2; exit 1 ;; \
    esac && \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz && \
    mkdir -p /opt/zig && \
    tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig && \
    rm /tmp/zig.tar.xz && \
    zig version

RUN curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | \
        sh -s -- -y --profile minimal --default-toolchain stable && \
    rustup target add "$RUST_TARGET" && \
    cargo install cargo-zigbuild --locked

RUN curl --proto '=https' --tlsv1.2 -fsSL https://bun.sh/install | bash && bun --version

WORKDIR /source
COPY . .

RUN test "$(bun scripts/product-version.cjs --print)" = "$ME_VERSION"
RUN cargo zigbuild --locked --release --bins --target "${RUST_TARGET}.2.17"
RUN packaging/linux/prepare-amd64-appimage-tools.sh
RUN cd me-client && bunx @tauri-apps/cli@2.11.3 build --no-bundle
RUN cd me-client && bunx @tauri-apps/cli@2.11.3 bundle --verbose --bundles appimage
RUN set -eu; \
    client=$(find me-client/src-tauri/target/release/bundle/appimage -maxdepth 1 -name '*.AppImage' -print -quit); \
    test -n "$client"; \
    mkdir -p /artifact; \
    packaging/linux/build-run.sh \
        "$ME_VERSION" \
        "$PACKAGE_ARCH" \
        "target/$RUST_TARGET/release/me-s" \
        "target/$RUST_TARGET/release/me-gateway" \
        "$client" \
        "/artifact/$ASSET_NAME"

FROM scratch
COPY --from=build /artifact/ /
