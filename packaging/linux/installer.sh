#!/bin/sh
set -eu

PRODUCT_VERSION='@ME_VERSION@'
INSTALL_DIR=${ME_INSTALL_DIR:-/usr/local/bin}
MODE=install
EXTRACT_DIR=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            printf 'ME product %s\n' "$PRODUCT_VERSION"
            exit 0
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { printf 'error: --install-dir requires a directory\n' >&2; exit 2; }
            INSTALL_DIR=$2
            shift 2
            ;;
        --extract-dir)
            [ "$#" -ge 2 ] || { printf 'error: --extract-dir requires a directory\n' >&2; exit 2; }
            MODE=extract
            EXTRACT_DIR=$2
            shift 2
            ;;
        --help)
            printf 'usage: %s [--install-dir <directory>] [--version]\n' "$0"
            exit 0
            ;;
        *)
            printf 'error: unsupported argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done

for command in awk chmod install mkdir mktemp mv rm tail tar; do
    command -v "$command" >/dev/null 2>&1 || { printf 'error: required command not found: %s\n' "$command" >&2; exit 1; }
done

ARCHIVE_LINE=$(awk '/^__ME_ARCHIVE_BELOW__$/ { print NR + 1; exit }' "$0")
[ -n "$ARCHIVE_LINE" ] || { printf 'error: installer payload marker is missing\n' >&2; exit 1; }
WORK=$(mktemp -d "${TMPDIR:-/tmp}/me-product.XXXXXX")
cleanup() { rm -rf "$WORK"; }
trap cleanup 0
trap 'exit 130' HUP INT TERM
tail -n +"$ARCHIVE_LINE" "$0" | tar -xzf - -C "$WORK"

for name in me-s me-gateway me-client; do
    [ -s "$WORK/$name" ] || { printf 'error: installer payload is missing %s\n' "$name" >&2; exit 1; }
    chmod 755 "$WORK/$name"
done
[ "$("$WORK/me-s" version)" = "me-s $PRODUCT_VERSION" ] || { printf 'error: me-s payload version mismatch\n' >&2; exit 1; }
[ "$("$WORK/me-gateway" version)" = "me-gateway $PRODUCT_VERSION" ] || { printf 'error: me-gateway payload version mismatch\n' >&2; exit 1; }

if [ "$MODE" = extract ]; then
    mkdir -p "$EXTRACT_DIR"
    for name in me-s me-gateway me-client; do
        install -m 755 "$WORK/$name" "$EXTRACT_DIR/$name"
    done
    exit 0
fi

install_product() {
    directory=$1
    transaction=$directory/.me-product-install-$$
    mkdir -p "$directory" "$transaction/new" "$transaction/old"
    for name in me-s me-gateway me-client; do
        install -m 755 "$WORK/$name" "$transaction/new/$name"
    done

    restore_backups() {
        for name in me-client me-gateway me-s; do
            if [ -f "$transaction/old/$name" ]; then
                mv "$transaction/old/$name" "$directory/$name"
            fi
        done
        rm -rf "$transaction"
    }

    rollback_commit() {
        for name in me-client me-gateway me-s; do
            rm -f "$directory/$name"
            if [ -f "$transaction/old/$name" ]; then
                mv "$transaction/old/$name" "$directory/$name"
            fi
        done
        rm -rf "$transaction"
    }

    for name in me-s me-gateway me-client; do
        if [ -e "$directory/$name" ] || [ -L "$directory/$name" ]; then
            [ -f "$directory/$name" ] && [ ! -L "$directory/$name" ] || {
                printf 'error: install target is not a regular file: %s/%s\n' "$directory" "$name" >&2
                restore_backups
                return 1
            }
            mv "$directory/$name" "$transaction/old/$name" || { restore_backups; return 1; }
        fi
    done
    for name in me-s me-gateway me-client; do
        mv "$transaction/new/$name" "$directory/$name" || { rollback_commit; return 1; }
    done
    rm -rf "$transaction"
}

if mkdir -p "$INSTALL_DIR" 2>/dev/null && [ -w "$INSTALL_DIR" ]; then
    install_product "$INSTALL_DIR"
else
    command -v sudo >/dev/null 2>&1 || { printf 'error: cannot write to %s and sudo is unavailable\n' "$INSTALL_DIR" >&2; exit 1; }
    printf 'Installing ME to %s requires administrator access.\n' "$INSTALL_DIR"
    exec sudo "$0" --install-dir "$INSTALL_DIR"
fi

printf 'Installed ME %s to %s\n' "$PRODUCT_VERSION" "$INSTALL_DIR"
printf '  me-s: %s/me-s\n' "$INSTALL_DIR"
printf '  me-gateway: %s/me-gateway\n' "$INSTALL_DIR"
printf '  me-client: %s/me-client\n' "$INSTALL_DIR"
exit 0

__ME_ARCHIVE_BELOW__
