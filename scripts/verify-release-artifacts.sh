#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR=${1:-"$ROOT_DIR/dist"}
VERSION="$(node "$ROOT_DIR/scripts/product-version.cjs" --print)"
PACKAGE_ASSETS=(
    ME-macos-universal.pkg
    ME-windows-x86_64-setup.exe
    ME-linux-x86_64.run
    ME-linux-arm64.run
)
EXPECTED_ASSETS=$'ME-linux-arm64.run\nME-linux-x86_64.run\nME-macos-universal.pkg\nME-windows-x86_64-setup.exe\nSHA256SUMS'

for command in file lipo node pkgutil shasum tar xcrun; do
    command -v "$command" >/dev/null 2>&1 || { echo "error: missing static verification dependency: $command" >&2; exit 1; }
done
if [[ -n "${ME_7Z:-}" ]]; then
    SEVENZIP=$ME_7Z
elif command -v 7zz >/dev/null 2>&1; then
    SEVENZIP=$(command -v 7zz)
elif command -v 7z >/dev/null 2>&1; then
    SEVENZIP=$(command -v 7z)
else
    echo "error: 7zz or 7z is required for static NSIS inspection" >&2
    exit 1
fi

[[ -d "$DIST_DIR" ]] || { echo "error: missing release directory: $DIST_DIR" >&2; exit 1; }
ACTUAL_ASSETS="$(find "$DIST_DIR" -maxdepth 1 -type f -exec basename {} \; | LC_ALL=C sort)"
if [[ "$ACTUAL_ASSETS" != "$EXPECTED_ASSETS" ]]; then
    printf 'error: release directory has an unexpected asset set\nexpected:\n%s\nactual:\n%s\n' "$EXPECTED_ASSETS" "$ACTUAL_ASSETS" >&2
    exit 1
fi

for asset in "${PACKAGE_ASSETS[@]}"; do
    bytes=$(wc -c <"$DIST_DIR/$asset" | tr -d ' ')
    (( bytes >= 1048576 )) || { echo "error: release package is unexpectedly small: $asset ($bytes bytes)" >&2; exit 1; }
done

MANIFEST="$DIST_DIR/SHA256SUMS"
[[ $(wc -l <"$MANIFEST" | tr -d ' ') -eq 4 ]] || { echo "error: SHA256SUMS must contain exactly four entries" >&2; exit 1; }
for asset in "${PACKAGE_ASSETS[@]}"; do
    [[ $(grep -Ec "^[0-9a-f]{64}  ${asset//./\\.}$" "$MANIFEST") -eq 1 ]] || {
        echo "error: SHA256SUMS must contain one exact entry for $asset" >&2
        exit 1
    }
done
(cd "$DIST_DIR" && shasum -a 256 -c SHA256SUMS >/dev/null)

WORK=$(mktemp -d "${TMPDIR:-/tmp}/me-release-static.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT HUP INT TERM

MAC_PACKAGE="$DIST_DIR/ME-macos-universal.pkg"
file "$MAC_PACKAGE" | grep -F 'xar archive' >/dev/null
MAC_PAYLOAD_RAW="$WORK/macos-payload-raw.txt"
MAC_PAYLOAD="$WORK/macos-payload.txt"
pkgutil --payload-files "$MAC_PACKAGE" >"$MAC_PAYLOAD_RAW"
# pkgbuild represents extended metadata as AppleDouble archive entries; they are not installed paths.
grep -Ev '(^|/)\._' "$MAC_PAYLOAD_RAW" >"$MAC_PAYLOAD"
for required in \
    './usr/local/bin/me-s' \
    './usr/local/bin/me-gateway' \
    './Applications/ME Client.app/Contents/MacOS/me-client'
do
    grep -Fx "$required" "$MAC_PAYLOAD" >/dev/null || { echo "error: macOS pkg is missing $required" >&2; exit 1; }
done
while IFS= read -r path; do
    case "$path" in
        .|./usr|./usr/local|./usr/local/bin|./Applications|./usr/local/bin/me-s|./usr/local/bin/me-gateway|./Applications/ME\ Client.app|./Applications/ME\ Client.app/*) ;;
        *) echo "error: unexpected macOS pkg payload path: $path" >&2; exit 1 ;;
    esac
done <"$MAC_PAYLOAD"
pkgutil --expand-full "$MAC_PACKAGE" "$WORK/macos-expanded" >/dev/null
for binary in \
    "$WORK/macos-expanded/Payload/usr/local/bin/me-s" \
    "$WORK/macos-expanded/Payload/usr/local/bin/me-gateway" \
    "$WORK/macos-expanded/Payload/Applications/ME Client.app/Contents/MacOS/me-client"
do
    [[ -s "$binary" ]] || { echo "error: missing macOS package binary: $binary" >&2; exit 1; }
    lipo "$binary" -verify_arch x86_64 arm64 >/dev/null
    file "$binary" | grep -F 'Mach-O universal binary' >/dev/null
done

WINDOWS_PACKAGE="$DIST_DIR/ME-windows-x86_64-setup.exe"
file "$WINDOWS_PACKAGE" | grep -F 'Nullsoft Installer self-extracting archive' >/dev/null
mkdir -p "$WORK/windows"
"$SEVENZIP" x -y -o"$WORK/windows" "$WINDOWS_PACKAGE" >/dev/null
WINDOWS_ROOT_FILES="$(find "$WORK/windows" -maxdepth 1 -type f -exec basename {} \; | LC_ALL=C sort)"
EXPECTED_WINDOWS_FILES=$'Uninstall ME.exe\nme-client.exe\nme-gateway.exe\nme-s.exe'
if [[ "$WINDOWS_ROOT_FILES" != "$EXPECTED_WINDOWS_FILES" ]]; then
    printf 'error: unexpected Windows installer root payload\nexpected:\n%s\nactual:\n%s\n' "$EXPECTED_WINDOWS_FILES" "$WINDOWS_ROOT_FILES" >&2
    exit 1
fi
for binary in me-s.exe me-gateway.exe me-client.exe; do
    file "$WORK/windows/$binary" | grep -E 'PE32\+ executable .* x86-64' >/dev/null
    xcrun llvm-objdump --file-headers "$WORK/windows/$binary" | grep -F 'file format coff-x86-64' >/dev/null
done
[[ -s "$WORK/windows/Uninstall ME.exe" ]] || { echo "error: Windows setup does not contain an uninstaller" >&2; exit 1; }

verify_linux_run() {
    local package=$1
    local expected_arch=$2
    local expected_file_pattern=$3
    local name
    name=$(basename "$package")
    [[ $(head -n 1 "$package") == '#!/bin/sh' ]] || { echo "error: $name does not start with a shell installer" >&2; exit 1; }
    local marker_line
    marker_line=$(LC_ALL=C awk '/^__ME_ARCHIVE_BELOW__$/ { print NR; exit }' "$package")
    [[ -n "$marker_line" ]] || { echo "error: $name has no payload marker" >&2; exit 1; }
    sed -n "1,${marker_line}p" "$package" | grep -Fx "PRODUCT_VERSION='$VERSION'" >/dev/null || {
        echo "error: $name does not embed product version $VERSION" >&2
        exit 1
    }
    local directory="$WORK/$expected_arch"
    mkdir -p "$directory/payload"
    tail -n "+$((marker_line + 1))" "$package" >"$directory/payload.tar.gz"
    local actual_files
    actual_files=$(tar -tzf "$directory/payload.tar.gz" | LC_ALL=C sort)
    local expected_files=$'me-client\nme-gateway\nme-s'
    [[ "$actual_files" == "$expected_files" ]] || { echo "error: $name payload file set is invalid" >&2; exit 1; }
    tar -xzf "$directory/payload.tar.gz" -C "$directory/payload"
    for binary in me-s me-gateway me-client; do
        [[ -x "$directory/payload/$binary" && -s "$directory/payload/$binary" ]] || {
            echo "error: $name contains an invalid $binary payload" >&2
            exit 1
        }
        file "$directory/payload/$binary" | grep -E "$expected_file_pattern" >/dev/null || {
            echo "error: $name contains $binary for the wrong architecture" >&2
            exit 1
        }
    done
    local appimage_magic
    appimage_magic=$(od -An -tx1 -j 8 -N 3 "$directory/payload/me-client" | tr -d ' \n')
    [[ "$appimage_magic" == 414902 ]] || { echo "error: $name me-client is not a type-2 AppImage" >&2; exit 1; }
}

verify_linux_run "$DIST_DIR/ME-linux-x86_64.run" x86_64 'ELF 64-bit LSB.*x86-64'
verify_linux_run "$DIST_DIR/ME-linux-arm64.run" arm64 'ELF 64-bit LSB.*ARM aarch64'

echo "static release artifact verification: PASS"
