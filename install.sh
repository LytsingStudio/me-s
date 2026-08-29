#!/bin/sh
set -eu

REPOSITORY="${ME_INSTALL_REPOSITORY:-LytsingStudio/me-s}"
BASE_URL="${ME_INSTALL_BASE_URL:-https://github.com/$REPOSITORY/releases/latest/download}"
INSTALL_DIR="${ME_INSTALL_DIR:-/usr/local/bin}"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

detect_product() {
    system=${ME_INSTALL_OS:-$(uname -s)}
    machine=${ME_INSTALL_ARCH:-$(uname -m)}
    case "$system" in
        Darwin|darwin|macOS|macos)
            case "$machine" in
                arm64|aarch64|x86_64|amd64) printf '%s %s\n' pkg ME-macos-universal.pkg ;;
                *) fail "ME does not provide a macOS package for $machine" ;;
            esac
            ;;
        Linux|linux)
            case "$machine" in
                arm64|aarch64) printf '%s %s\n' run ME-linux-arm64.run ;;
                x86_64|amd64) printf '%s %s\n' run ME-linux-x86_64.run ;;
                *) fail "ME does not provide a Linux package for $machine" ;;
            esac
            ;;
        *) fail "ME does not support $system/$machine with this installer" ;;
    esac
}

download() {
    url=$1
    output=$2
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error \
            --retry 3 --connect-timeout 15 --max-time 1800 \
            --output "$output" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --quiet --tries=3 --timeout=30 --output-document="$output" "$url"
    else
        fail 'curl or wget is required to download ME'
    fi
}

expected_checksum() {
    manifest=$1
    asset=$2
    awk -v asset="$asset" '
        NF == 2 {
            file = $2
            sub(/^\*/, "", file)
            if (file == asset) {
                count++
                checksum = tolower($1)
            }
        }
        END {
            if (count != 1 || length(checksum) != 64 || checksum !~ /^[0-9a-f]+$/) exit 1
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
        fail 'sha256sum, shasum, or openssl is required to verify ME'
    fi
}

validate_cli_product() {
    directory=$1
    me_s_output=$("$directory/me-s" version) || fail 'installed me-s could not be started'
    gateway_output=$("$directory/me-gateway" version) || fail 'installed me-gateway could not be started'
    case "$me_s_output" in 'me-s '*) version=${me_s_output#me-s } ;; *) fail 'installed me-s reported an invalid identity' ;; esac
    [ "$gateway_output" = "me-gateway $version" ] || fail 'installed ME programs report different versions'
    printf '%s\n%s\n' "$me_s_output" "$gateway_output"
}

cleanup() {
    [ -n "${TEMP_DIR:-}" ] && [ -d "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
}

need_command awk
need_command mktemp
need_command rm
need_command uname

set -- $(detect_product)
PACKAGE_KIND=$1
PACKAGE_ASSET=$2
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-install.XXXXXX") || fail 'cannot create temporary directory'
trap cleanup 0
trap 'exit 130' HUP INT TERM
PACKAGE=$TEMP_DIR/$PACKAGE_ASSET
MANIFEST=$TEMP_DIR/SHA256SUMS

printf 'Downloading %s...\n' "$PACKAGE_ASSET"
download "$BASE_URL/$PACKAGE_ASSET" "$PACKAGE"
download "$BASE_URL/SHA256SUMS" "$MANIFEST"
expected=$(expected_checksum "$MANIFEST" "$PACKAGE_ASSET") || fail "SHA256SUMS has no single valid entry for $PACKAGE_ASSET"
actual=$(actual_checksum "$PACKAGE")
[ "$expected" = "$actual" ] || fail "checksum verification failed for $PACKAGE_ASSET"
printf 'Checksum verified.\n'

case "$PACKAGE_KIND" in
    pkg)
        [ "$INSTALL_DIR" = /usr/local/bin ] || fail 'the macOS product package installs ME to /usr/local/bin and does not support ME_INSTALL_DIR'
        need_command installer
        need_command sudo
        printf 'Installing ME requires administrator access.\n'
        sudo installer -pkg "$PACKAGE" -target /
        validate_cli_product /usr/local/bin
        [ -x '/Applications/ME Client.app/Contents/MacOS/me-client' ] || fail 'ME Client was not installed'
        printf 'Installed ME Client: /Applications/ME Client.app\n'
        ;;
    run)
        chmod 755 "$PACKAGE"
        "$PACKAGE" --install-dir "$INSTALL_DIR"
        validate_cli_product "$INSTALL_DIR"
        [ -x "$INSTALL_DIR/me-client" ] || fail 'ME Client was not installed'
        ;;
    *) fail "unsupported package kind: $PACKAGE_KIND" ;;
esac

printf 'ME installation completed.\n'
