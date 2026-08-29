#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
    echo "usage: $0 <version> <me-s.exe> <me-gateway.exe> <me-client.exe> <output.exe>" >&2
    exit 2
fi

VERSION=$1
ME_S=$2
ME_GATEWAY=$3
ME_CLIENT=$4
OUTPUT=$5
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT_DIR/packaging/windows/installer.nsi"
ICON="$ROOT_DIR/me-client/src-tauri/icons/icon.ico"

case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "error: Windows installer requires a stable three-part version: $VERSION" >&2; exit 1 ;;
esac
for input in "$ME_S" "$ME_GATEWAY" "$ME_CLIENT" "$SCRIPT" "$ICON"; do
    [[ -f "$input" ]] || { echo "error: missing installer input: $input" >&2; exit 1; }
done
command -v makensis >/dev/null 2>&1 || { echo "error: makensis is required" >&2; exit 1; }

absolute_file() {
    local path=$1
    local directory
    directory="$(cd "$(dirname "$path")" && pwd)"
    printf '%s/%s\n' "$directory" "$(basename "$path")"
}

ME_S="$(absolute_file "$ME_S")"
ME_GATEWAY="$(absolute_file "$ME_GATEWAY")"
ME_CLIENT="$(absolute_file "$ME_CLIENT")"
ICON="$(absolute_file "$ICON")"
mkdir -p "$(dirname "$OUTPUT")"
OUTPUT="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"
rm -f "$OUTPUT"

makensis -V2 \
    "-DVERSION=$VERSION" \
    "-DME_S=$ME_S" \
    "-DME_GATEWAY=$ME_GATEWAY" \
    "-DME_CLIENT=$ME_CLIENT" \
    "-DOUTPUT=$OUTPUT" \
    "-DICON=$ICON" \
    "$SCRIPT"

[[ -s "$OUTPUT" ]] || { echo "error: makensis did not create $OUTPUT" >&2; exit 1; }
echo "built $OUTPUT"
