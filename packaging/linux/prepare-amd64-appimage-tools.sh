#!/usr/bin/env bash
set -euo pipefail

TOOLS_DIR=${XDG_CACHE_HOME:-${HOME}/.cache}/tauri
WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM
mkdir -p "$TOOLS_DIR"

case "${TARGETARCH:-}" in
    amd64)
        tools_arch=x86_64
        ;;
    arm64)
        tools_arch=aarch64
        ;;
    *)
        printf 'unsupported Docker target architecture: %s\n' "${TARGETARCH:-}" >&2
        exit 1
        ;;
esac

download() {
    local url=$1
    local output=$2
    if [[ ! -s "$output" ]]; then
        curl -fsSL "$url" -o "$output"
    fi
    chmod 755 "$output"
}

download \
    "https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-${tools_arch}" \
    "$TOOLS_DIR/AppRun-${tools_arch}"
download \
    https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh \
    "$TOOLS_DIR/linuxdeploy-plugin-gtk.sh"
download \
    https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gstreamer/master/linuxdeploy-plugin-gstreamer.sh \
    "$TOOLS_DIR/linuxdeploy-plugin-gstreamer.sh"

LINUXDEPLOY_ROOT=/opt/me-linuxdeploy
PLUGIN_ROOT=/opt/me-linuxdeploy-plugin-appimage
extract_appimage() {
    local image=$1
    local destination=$2
    local offset=
    local candidate
    for candidate in $(LC_ALL=C grep -abo hsqs "$image" | cut -d: -f1); do
        if unsquashfs -s -o "$candidate" "$image" >/dev/null 2>&1; then
            offset=$candidate
            break
        fi
    done
    [[ -n "$offset" ]]
    rm -rf "$destination"
    unsquashfs -q -o "$offset" -d "$destination" "$image"
}

if [[ "$TARGETARCH" == arm64 ]]; then
    download \
        https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-aarch64.AppImage \
        "$TOOLS_DIR/linuxdeploy-aarch64.AppImage"
    download \
        https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-aarch64.AppImage \
        "$WORK/linuxdeploy-plugin-appimage.AppImage"
    extract_appimage "$WORK/linuxdeploy-plugin-appimage.AppImage" "$PLUGIN_ROOT"
    download \
        https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-aarch64 \
        "$PLUGIN_ROOT/runtime-aarch64"
    printf '%s  %s\n' \
        7d5d772b7c32f0c84caf0a452a3072a5709027d7eac5856feb89a7a7a8881372 \
        "$PLUGIN_ROOT/runtime-aarch64" | sha256sum --check -
    mv "$PLUGIN_ROOT/usr/bin/appimagetool" "$PLUGIN_ROOT/usr/bin/appimagetool.real"
    cat >"$PLUGIN_ROOT/usr/bin/appimagetool" <<'EOF'
#!/bin/sh
exec /opt/me-linuxdeploy-plugin-appimage/usr/bin/appimagetool.real \
    --runtime-file /opt/me-linuxdeploy-plugin-appimage/runtime-aarch64 "$@"
EOF
    chmod 755 "$PLUGIN_ROOT/usr/bin/appimagetool"
    install -m 755 \
        "$PLUGIN_ROOT/usr/bin/linuxdeploy-plugin-appimage" \
        "$TOOLS_DIR/linuxdeploy-plugin-appimage.AppImage"
    PATH="$PLUGIN_ROOT/usr/bin:$TOOLS_DIR:$PATH"
    export PATH
    "$TOOLS_DIR/linuxdeploy-aarch64.AppImage" --appimage-extract-and-run --version >/dev/null
    "$TOOLS_DIR/linuxdeploy-plugin-appimage.AppImage" --plugin-type >/dev/null
    exit 0
fi

download \
    https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-x86_64.AppImage \
    "$WORK/linuxdeploy.AppImage"
download \
    https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage \
    "$WORK/linuxdeploy-plugin-appimage.AppImage"
extract_appimage "$WORK/linuxdeploy.AppImage" "$LINUXDEPLOY_ROOT"
extract_appimage "$WORK/linuxdeploy-plugin-appimage.AppImage" "$PLUGIN_ROOT"

# Rosetta cannot launch the outer AppImage runtime used by these amd64 build tools.
cat >"$WORK/linuxdeploy-wrapper.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    const char *target = "/opt/me-linuxdeploy/usr/bin/linuxdeploy";
    const char *path = getenv("PATH");
    char combined[8192];
    if (snprintf(combined, sizeof(combined), "/root/.cache/tauri:%s", path ? path : "") >= (int)sizeof(combined) ||
        setenv("PATH", combined, 1) != 0) {
        perror("prepare linuxdeploy PATH");
        return 127;
    }
    if (argc > 1 && strcmp(argv[1], "--appimage-extract-and-run") == 0) {
        argv[1] = (char *)target;
        execv(target, &argv[1]);
    } else {
        argv[0] = (char *)target;
        execv(target, argv);
    }
    perror("execv linuxdeploy");
    return 127;
}
EOF
cc -O2 -o "$TOOLS_DIR/linuxdeploy-x86_64.AppImage" "$WORK/linuxdeploy-wrapper.c"
install -m 755 \
    "$PLUGIN_ROOT/usr/bin/linuxdeploy-plugin-appimage" \
    "$TOOLS_DIR/linuxdeploy-plugin-appimage.AppImage"

PATH="$PLUGIN_ROOT/usr/bin:$TOOLS_DIR:$PATH"
export PATH
"$TOOLS_DIR/linuxdeploy-x86_64.AppImage" --appimage-extract-and-run --version >/dev/null
"$TOOLS_DIR/linuxdeploy-plugin-appimage.AppImage" --plugin-type >/dev/null
