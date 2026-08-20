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

detect_platform() {
    system=${ME_INSTALL_OS:-$(uname -s)}
    machine=${ME_INSTALL_ARCH:-$(uname -m)}

    case "$system" in
        Darwin|darwin|macOS|macos)
            case "$machine" in
                arm64|aarch64) printf '%s\n' 'macos-arm64' ;;
                x86_64|amd64) printf '%s\n' 'macos-x86_64' ;;
                *) fail "ME does not provide a macOS release for $machine" ;;
            esac
            ;;
        Linux|linux)
            case "$machine" in
                arm64|aarch64) printf '%s\n' 'linux-arm64' ;;
                x86_64|amd64) printf '%s\n' 'linux-x86_64' ;;
                *) fail "ME does not provide a Linux release for $machine" ;;
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
            --retry 3 --connect-timeout 15 --max-time 600 \
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
        fail 'sha256sum, shasum, or openssl is required to verify ME'
    fi
}

cleanup() {
    if [ -n "${TEMP_DIR:-}" ] && [ -d "$TEMP_DIR" ]; then
        rm -rf "$TEMP_DIR"
    fi
}

write_transaction_helper() {
    cat >"$TEMP_DIR/install-product.sh" <<'HELPER'
#!/bin/sh
set -eu

source_me_s=$1
source_gateway=$2
destination_me_s=$3
destination_gateway=$4
install_directory=$5
expected_me_s=$6
expected_gateway=$7
mkdir -p "$install_directory"
for destination in "$destination_me_s" "$destination_gateway"; do
    if [ -L "$destination" ] || { [ -e "$destination" ] && [ ! -f "$destination" ]; }; then
        printf 'error: install target is not a regular file: %s\n' "$destination" >&2
        exit 1
    fi
done

transaction=$(mktemp -d "$install_directory/.me-install.XXXXXX")
had_me_s=0
had_gateway=0
installed_me_s=0
installed_gateway=0
committed=0

finish() {
    status=$?
    trap - 0
    set +e
    rollback_failed=0
    if [ "$status" -ne 0 ] && [ "$committed" -eq 0 ]; then
        if [ "$installed_gateway" -eq 1 ] && ! rm -f "$destination_gateway"; then
            printf 'error: rollback could not remove %s\n' "$destination_gateway" >&2
            rollback_failed=1
        fi
        if [ "$installed_me_s" -eq 1 ] && ! rm -f "$destination_me_s"; then
            printf 'error: rollback could not remove %s\n' "$destination_me_s" >&2
            rollback_failed=1
        fi
        if [ "$had_gateway" -eq 1 ]; then
            if [ -e "$transaction/me-gateway.old" ]; then
                if ! mv -f "$transaction/me-gateway.old" "$destination_gateway"; then rollback_failed=1; fi
            else
                rollback_failed=1
            fi
        fi
        if [ "$had_me_s" -eq 1 ]; then
            if [ -e "$transaction/me-s.old" ]; then
                if ! mv -f "$transaction/me-s.old" "$destination_me_s"; then rollback_failed=1; fi
            else
                rollback_failed=1
            fi
        fi
    fi
    if [ "$rollback_failed" -eq 0 ]; then
        rm -rf "$transaction"
    else
        printf 'error: rollback also failed; recovery files remain in %s\n' "$transaction" >&2
    fi
    exit "$status"
}
trap finish 0
trap 'exit 130' HUP INT TERM

install -m 755 "$source_me_s" "$transaction/me-s.new"
install -m 755 "$source_gateway" "$transaction/me-gateway.new"
if [ -f "$destination_me_s" ]; then
    mv -f "$destination_me_s" "$transaction/me-s.old"
    had_me_s=1
fi
if [ -f "$destination_gateway" ]; then
    mv -f "$destination_gateway" "$transaction/me-gateway.old"
    had_gateway=1
fi
mv -f "$transaction/me-s.new" "$destination_me_s"
installed_me_s=1
mv -f "$transaction/me-gateway.new" "$destination_gateway"
installed_gateway=1

actual_me_s=$("$destination_me_s" version)
[ "$actual_me_s" = "$expected_me_s" ] || {
    printf 'error: installed me-s reported an unexpected version\n' >&2
    exit 1
}
actual_gateway=$("$destination_gateway" version)
[ "$actual_gateway" = "$expected_gateway" ] || {
    printf 'error: installed me-gateway reported an unexpected version\n' >&2
    exit 1
}
printf '%s\n' "$actual_me_s"
printf '%s\n' "$actual_gateway"
committed=1
rm -f "$transaction/me-s.old" "$transaction/me-gateway.old" || true
HELPER
    chmod 755 "$TEMP_DIR/install-product.sh"
}

install_product() {
    source_me_s=$1
    source_gateway=$2
    destination_me_s=$3
    destination_gateway=$4
    directory=$5
    expected_me_s=$6
    expected_gateway=$7

    if mkdir -p "$directory" 2>/dev/null && [ -w "$directory" ]; then
        sh "$TEMP_DIR/install-product.sh" \
            "$source_me_s" "$source_gateway" \
            "$destination_me_s" "$destination_gateway" "$directory" \
            "$expected_me_s" "$expected_gateway"
        return
    fi

    command -v sudo >/dev/null 2>&1 || \
        fail "cannot write to $directory; set ME_INSTALL_DIR to a writable directory"
    printf 'Installing to %s requires administrator access.\n' "$directory"
    sudo sh "$TEMP_DIR/install-product.sh" \
        "$source_me_s" "$source_gateway" \
        "$destination_me_s" "$destination_gateway" "$directory" \
        "$expected_me_s" "$expected_gateway"
}

need_command uname
need_command awk
need_command chmod
need_command install
need_command mktemp
need_command mv
need_command rm
need_command sh

PLATFORM=$(detect_platform)
ME_S_ASSET="me-s-$PLATFORM"
GATEWAY_ASSET="me-gateway-$PLATFORM"
TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-install.XXXXXX") || fail 'cannot create temporary directory'
trap cleanup 0
trap 'exit 130' HUP INT TERM

printf 'Downloading %s and %s...\n' "$ME_S_ASSET" "$GATEWAY_ASSET"
download "$BASE_URL/$ME_S_ASSET" "$TEMP_DIR/$ME_S_ASSET"
download "$BASE_URL/$GATEWAY_ASSET" "$TEMP_DIR/$GATEWAY_ASSET"
download "$BASE_URL/SHA256SUMS" "$TEMP_DIR/SHA256SUMS"

for asset in "$ME_S_ASSET" "$GATEWAY_ASSET"; do
    expected=$(expected_checksum "$TEMP_DIR/SHA256SUMS" "$asset") || \
        fail "SHA256SUMS has no single valid entry for $asset"
    actual=$(actual_checksum "$TEMP_DIR/$asset")
    [ "$expected" = "$actual" ] || fail "checksum verification failed for $asset"
done
printf 'Checksums verified.\n'

chmod 755 "$TEMP_DIR/$ME_S_ASSET" "$TEMP_DIR/$GATEWAY_ASSET"
ME_S_VERSION_OUTPUT=$("$TEMP_DIR/$ME_S_ASSET" version) || \
    fail "downloaded $ME_S_ASSET cannot run on this system"
GATEWAY_VERSION_OUTPUT=$("$TEMP_DIR/$GATEWAY_ASSET" version) || \
    fail "downloaded $GATEWAY_ASSET cannot run on this system"
case "$ME_S_VERSION_OUTPUT" in
    'me-s '*) ME_S_VERSION=${ME_S_VERSION_OUTPUT#me-s } ;;
    *) fail "downloaded $ME_S_ASSET did not identify itself as me-s" ;;
esac
case "$GATEWAY_VERSION_OUTPUT" in
    'me-gateway '*) GATEWAY_VERSION=${GATEWAY_VERSION_OUTPUT#me-gateway } ;;
    *) fail "downloaded $GATEWAY_ASSET did not identify itself as me-gateway" ;;
esac
case "$ME_S_VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "downloaded $ME_S_ASSET reported an invalid version" ;;
esac
case "$ME_S_VERSION" in ''|*[!0-9A-Za-z.+-]*) fail "downloaded $ME_S_ASSET reported an invalid version" ;; esac
[ "$ME_S_VERSION" = "$GATEWAY_VERSION" ] || fail 'downloaded ME programs report different versions'

write_transaction_helper
ME_S_DESTINATION=$INSTALL_DIR/me-s
GATEWAY_DESTINATION=$INSTALL_DIR/me-gateway
install_product \
    "$TEMP_DIR/$ME_S_ASSET" "$TEMP_DIR/$GATEWAY_ASSET" \
    "$ME_S_DESTINATION" "$GATEWAY_DESTINATION" "$INSTALL_DIR" \
    "$ME_S_VERSION_OUTPUT" "$GATEWAY_VERSION_OUTPUT"

printf 'Installed ME to %s\n' "$INSTALL_DIR"
printf '  me-s: %s\n' "$ME_S_DESTINATION"
printf '  me-gateway: %s\n' "$GATEWAY_DESTINATION"
