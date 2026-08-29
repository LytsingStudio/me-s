#!/bin/sh
set -eu

if [ "${TARGETARCH:-}" != "amd64" ]; then
    exit 0
fi

TOOLS_DIR=${XDG_CACHE_HOME:-${HOME}/.cache}/tauri
LINUXDEPLOY_ROOT=/opt/me-linuxdeploy
PLUGIN_ROOT=/opt/me-linuxdeploy-plugin-appimage
WORK=$(mktemp -d)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM
mkdir -p "$TOOLS_DIR"

extract_appimage() {
    image=$1
    destination=$2
    offset=
    for candidate in $(LC_ALL=C grep -abo hsqs "$image" | cut -d: -f1); do
        if unsquashfs -s -o "$candidate" "$image" >/dev/null 2>&1; then
            offset=$candidate
            break
        fi
    done
    test -n "$offset"
    unsquashfs -q -o "$offset" -d "$destination" "$image"
}

curl -fsSL \
    https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-x86_64.AppImage \
    -o "$WORK/linuxdeploy.AppImage"
curl -fsSL \
    https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage \
    -o "$WORK/linuxdeploy-plugin-appimage.AppImage"
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
