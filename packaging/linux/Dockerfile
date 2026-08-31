# syntax=docker/dockerfile:1
FROM ubuntu:22.04 AS build

ARG TARGETARCH
ARG ME_VERSION
ARG RUST_TARGET
ARG PACKAGE_ARCH
ARG ASSET_NAME

ENV ME_VERSION=$ME_VERSION \
    RUST_TARGET=$RUST_TARGET \
    PACKAGE_ARCH=$PACKAGE_ARCH \
    ASSET_NAME=$ASSET_NAME \
    TARGETARCH=$TARGETARCH \
    ME_SOURCE_DIR=/source

COPY . /source
COPY packaging/linux/build-in-container.sh /usr/local/bin/me-linux-build
RUN bash /usr/local/bin/me-linux-build

FROM scratch
COPY --from=build /artifact/ /
