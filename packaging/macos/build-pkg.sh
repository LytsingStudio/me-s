#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
    printf 'usage: %s <version> <me-s> <me-gateway> <ME Client.app> <output.pkg>\n' "$0" >&2
    exit 2
fi

VERSION=$1
ME_S=$2
ME_GATEWAY=$3
ME_CLIENT_APP=$4
OUTPUT=$5

case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) printf 'error: invalid ME version: %s\n' "$VERSION" >&2; exit 1 ;;
esac
for file in "$ME_S" "$ME_GATEWAY"; do
    [ -f "$file" ] || { printf 'error: missing executable: %s\n' "$file" >&2; exit 1; }
done
[ -d "$ME_CLIENT_APP" ] || { printf 'error: missing application bundle: %s\n' "$ME_CLIENT_APP" >&2; exit 1; }
command -v ditto >/dev/null 2>&1 || { printf 'error: ditto is required\n' >&2; exit 1; }
command -v pkgbuild >/dev/null 2>&1 || { printf 'error: pkgbuild is required\n' >&2; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/me-pkg.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup 0
trap 'exit 130' HUP INT TERM
ROOT=$WORK/root
mkdir -p "$ROOT/Applications" "$ROOT/usr/local/bin"
ditto "$ME_CLIENT_APP" "$ROOT/Applications/ME Client.app"
install -m 755 "$ME_S" "$ROOT/usr/local/bin/me-s"
install -m 755 "$ME_GATEWAY" "$ROOT/usr/local/bin/me-gateway"

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"
pkgbuild \
    --root "$ROOT" \
    --identifier studio.lytsing.me \
    --version "$VERSION" \
    --install-location / \
    --ownership recommended \
    "$OUTPUT"

[ -s "$OUTPUT" ] || { printf 'error: pkgbuild did not create %s\n' "$OUTPUT" >&2; exit 1; }
pkgutil --payload-files "$OUTPUT" | grep -Fx './usr/local/bin/me-s' >/dev/null
pkgutil --payload-files "$OUTPUT" | grep -Fx './usr/local/bin/me-gateway' >/dev/null
pkgutil --payload-files "$OUTPUT" | grep -F './Applications/ME Client.app/Contents/MacOS/me-client' >/dev/null
printf 'built %s\n' "$OUTPUT"
