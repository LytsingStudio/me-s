#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
    printf 'usage: %s <version> <arch> <me-s> <me-gateway> <me-client.AppImage> <output.run>\n' "$0" >&2
    exit 2
fi

VERSION=$1
ARCH=$2
ME_S=$3
ME_GATEWAY=$4
ME_CLIENT=$5
OUTPUT=$6
ROOT=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
TEMPLATE=$ROOT/packaging/linux/installer.sh

case "$VERSION" in [0-9]*.[0-9]*.[0-9]*) ;; *) printf 'error: invalid version: %s\n' "$VERSION" >&2; exit 1 ;; esac
case "$ARCH" in x86_64|arm64) ;; *) printf 'error: unsupported Linux architecture: %s\n' "$ARCH" >&2; exit 1 ;; esac
for file in "$ME_S" "$ME_GATEWAY" "$ME_CLIENT" "$TEMPLATE"; do
    [ -f "$file" ] || { printf 'error: missing input: %s\n' "$file" >&2; exit 1; }
done

WORK=$(mktemp -d "${TMPDIR:-/tmp}/me-run.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup 0
trap 'exit 130' HUP INT TERM
mkdir -p "$WORK/payload" "$WORK/check"
install -m 755 "$ME_S" "$WORK/payload/me-s"
install -m 755 "$ME_GATEWAY" "$WORK/payload/me-gateway"
install -m 755 "$ME_CLIENT" "$WORK/payload/me-client"
tar -czf "$WORK/payload.tar.gz" -C "$WORK/payload" me-s me-gateway me-client

[ "$(grep -c '^__ME_ARCHIVE_BELOW__$' "$TEMPLATE")" -eq 1 ] || {
    printf 'error: Linux installer template must contain exactly one payload marker\n' >&2
    exit 1
}
mkdir -p "$(dirname "$OUTPUT")"
sed "s/@ME_VERSION@/$VERSION/g" "$TEMPLATE" >"$OUTPUT"
cat "$WORK/payload.tar.gz" >>"$OUTPUT"
chmod 755 "$OUTPUT"

archive_size=$(wc -c <"$WORK/payload.tar.gz" | tr -d ' ')
tail -c "$archive_size" "$OUTPUT" >"$WORK/check/payload.tar.gz"
cmp "$WORK/payload.tar.gz" "$WORK/check/payload.tar.gz"
actual_files=$(tar -tzf "$WORK/check/payload.tar.gz" | LC_ALL=C sort)
expected_files=$(printf '%s\n' me-client me-gateway me-s)
[ "$actual_files" = "$expected_files" ] || {
    printf 'error: Linux package payload is not exactly me-s, me-gateway, and me-client\n' >&2
    exit 1
}
tar -xzf "$WORK/check/payload.tar.gz" -C "$WORK/check"
cmp "$WORK/payload/me-s" "$WORK/check/me-s"
cmp "$WORK/payload/me-gateway" "$WORK/check/me-gateway"
cmp "$WORK/payload/me-client" "$WORK/check/me-client"
printf 'built ME Linux %s package: %s\n' "$ARCH" "$OUTPUT"
