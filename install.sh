#!/bin/sh
set -eu

REPOSITORY="${ME_INSTALL_REPOSITORY:-LytsingStudio/me-rust}"
BASE_URL="${ME_INSTALL_BASE_URL:-https://github.com/$REPOSITORY/releases/latest/download}"
INSTALL_DIR="${ME_INSTALL_DIR:-/usr/local/bin}"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

detect_asset() {
    system=${ME_INSTALL_OS:-$(uname -s)}
    machine=${ME_INSTALL_ARCH:-$(uname -m)}

    case "$system" in
        Darwin|darwin|macOS|macos)
            case "$machine" in
                arm64|aarch64) printf '%s\n' 'me-s-macos-arm64' ;;
                x86_64|amd64) printf '%s\n' 'me-s-macos-x86_64' ;;
                *) fail "me-s does not provide a macOS release for $machine" ;;
            esac
            ;;
        Linux|linux)
            case "$machine" in
                arm64|aarch64) printf '%s\n' 'me-s-linux-arm64' ;;
                x86_64|amd64) printf '%s\n' 'me-s-linux-x86_64' ;;
                *) fail "me-s does not provide a Linux release for $machine" ;;
            esac
            ;;
        *) fail "me-s does not support $system/$machine with this installer" ;;
    esac
}

download() {
    url=$1
    output=$2
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error \
            --retry 3 --connect-timeout 15 --max-time 600 \
            --output "$output" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --quiet --tries=3 --timeout=30 --output-document="$output" "$url"
    else
        fail 'curl or wget is required to download me-s'
    fi
}

expected_checksum() {
    manifest=$1
    asset=$2
    awk -v asset="$asset" '
        NF >= 2 {
            file = $2
            sub(/^\*/, "", file)
            if (file == asset) {
                count++
                checksum = tolower($1)
            }
        }
        END {
            if (count != 1 || length(checksum) != 64 || checksum !~ /^[0-9a-f]+$/) {
                exit 1
            }
            print checksum
        }
    ' "$manifest"
}

actual_checksum() {
    file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{ print tolower($1) }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{ print tolower($1) }'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file" | awk '{ print tolower($NF) }'
    else
        fail 'sha256sum, shasum, or openssl is required to verify me-s'
    fi
}

cleanup() {
    if [ -n "${TEMP_DIR:-}" ] && [ -d "$TEMP_DIR" ]; then
        rm -f "$TEMP_DIR/release-asset" "$TEMP_DIR/SHA256SUMS"
        rmdir "$TEMP_DIR" 2>/dev/null || true
    fi
}

install_binary() {
    source_file=$1
    destination=$2
    directory=$3

    if mkdir -p "$directory" 2>/dev/null && [ -w "$directory" ]; then
        install -m 755 "$source_file" "$destination"
        return
    fi

    command -v sudo >/dev/null 2>&1 || \
        fail "cannot write to $directory; set ME_INSTALL_DIR to a writable directory"
    printf 'Installing to %s requires administrator access.\n' "$directory"
    sudo install -d -m 755 "$directory"
    sudo install -m 755 "$source_file" "$destination"
}

need_command uname
need_command awk
need_command chmod
need_command install
need_command mktemp

ASSET=$(detect_asset)
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-s-install.XXXXXX") || fail 'cannot create temporary directory'
trap cleanup 0
trap 'exit 130' HUP INT TERM

printf 'Downloading %s...\n' "$ASSET"
download "$BASE_URL/$ASSET" "$TEMP_DIR/release-asset"
download "$BASE_URL/SHA256SUMS" "$TEMP_DIR/SHA256SUMS"

EXPECTED=$(expected_checksum "$TEMP_DIR/SHA256SUMS" "$ASSET") || \
    fail "SHA256SUMS has no single valid entry for $ASSET"
ACTUAL=$(actual_checksum "$TEMP_DIR/release-asset")
[ "$EXPECTED" = "$ACTUAL" ] || fail "checksum verification failed for $ASSET"
printf 'Checksum verified.\n'

chmod 755 "$TEMP_DIR/release-asset"
if ! "$TEMP_DIR/release-asset" version >/dev/null; then
    fail "downloaded $ASSET cannot run on this system"
fi

DESTINATION=$INSTALL_DIR/me-s
install_binary "$TEMP_DIR/release-asset" "$DESTINATION" "$INSTALL_DIR"

if ! "$DESTINATION" version; then
    fail "me-s was installed to $DESTINATION but could not be started"
fi

printf 'Installed me-s to %s\n' "$DESTINATION"
