#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-s-install-test.XXXXXX")
RELEASE_DIR=$TEST_DIR/release
mkdir -p "$RELEASE_DIR"

cleanup() {
    if [ -d "$TEST_DIR" ]; then
        find "$TEST_DIR" -type f -exec rm -f {} \;
        find "$TEST_DIR" -depth -type d -exec rmdir {} \; 2>/dev/null || true
    fi
}
trap cleanup 0
trap 'exit 130' HUP INT TERM

for asset in \
    me-s-macos-arm64 \
    me-s-macos-x86_64 \
    me-s-linux-arm64 \
    me-s-linux-x86_64
do
    printf '#!/bin/sh\nprintf "me-s test-%s\\n"\n' "$asset" >"$RELEASE_DIR/$asset"
    chmod 755 "$RELEASE_DIR/$asset"
done

(
    cd "$RELEASE_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum me-s-* >SHA256SUMS
    else
        shasum -a 256 me-s-* >SHA256SUMS
    fi
)

run_case() {
    os=$1
    arch=$2
    expected=$3
    install_dir=$TEST_DIR/install-$os-$arch
    mkdir -p "$install_dir"
    printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$install_dir/me"
    chmod 755 "$install_dir/me"
    output=$(
        ME_INSTALL_OS=$os \
        ME_INSTALL_ARCH=$arch \
        ME_INSTALL_BASE_URL="file://$RELEASE_DIR" \
        ME_INSTALL_DIR="$install_dir" \
        sh "$ROOT_DIR/install.sh"
    )
    printf '%s\n' "$output" | grep -F "me-s test-$expected" >/dev/null
    "$install_dir/me-s" version | grep -F "me-s test-$expected" >/dev/null
    "$install_dir/me" | grep -F 'legacy me' >/dev/null
}

run_case Darwin arm64 me-s-macos-arm64
run_case Darwin x86_64 me-s-macos-x86_64
run_case Linux aarch64 me-s-linux-arm64
run_case Linux amd64 me-s-linux-x86_64

bad_release=$TEST_DIR/bad-release
bad_install=$TEST_DIR/bad-install
mkdir -p "$bad_release" "$bad_install"
cp "$RELEASE_DIR/me-s-linux-x86_64" "$bad_release/me-s-linux-x86_64"
printf '%064d  me-s-linux-x86_64\n' 0 >"$bad_release/SHA256SUMS"
printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$bad_install/me"
printf '#!/bin/sh\nprintf "old me-s\\n"\n' >"$bad_install/me-s"
chmod 755 "$bad_install/me" "$bad_install/me-s"
if ME_INSTALL_OS=Linux \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$bad_release" \
    ME_INSTALL_DIR="$bad_install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'checksum failure unexpectedly installed me-s\n' >&2
    exit 1
fi
"$bad_install/me" | grep -F 'legacy me' >/dev/null
"$bad_install/me-s" | grep -F 'old me-s' >/dev/null

if ME_INSTALL_OS=Plan9 \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$RELEASE_DIR" \
    ME_INSTALL_DIR="$TEST_DIR/unsupported" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'unsupported platform unexpectedly installed me-s\n' >&2
    exit 1
fi

printf 'install.sh integration tests: PASS\n'
