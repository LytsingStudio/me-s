#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
grep -F 'LytsingStudio/me-s' "$ROOT_DIR/install.sh" >/dev/null
if grep -F 'LytsingStudio/me-rust' "$ROOT_DIR/install.sh" >/dev/null; then
    printf 'install.sh still targets the legacy me-rust release repository\n' >&2
    exit 1
fi

grep -F 'me-gateway-$PLATFORM' "$ROOT_DIR/install.sh" >/dev/null

bash -n "$ROOT_DIR/release.sh"
for asset in \
    me-gateway-macos-arm64 \
    me-gateway-macos-x86_64 \
    me-gateway-linux-arm64 \
    me-gateway-linux-x86_64 \
    me-gateway-windows-x86_64.exe
do
    grep -F "$asset" "$ROOT_DIR/release.sh" >/dev/null
done
grep -F 'cargo build --locked --release --bins' "$ROOT_DIR/release.sh" >/dev/null
grep -F 'cargo zigbuild --locked --release --bins' "$ROOT_DIR/release.sh" >/dev/null
grep -F 'me-gateway-windows-x86_64.exe' "$ROOT_DIR/install.ps1" >/dev/null
grep -F '$installed = @($false, $false)' "$ROOT_DIR/install.ps1" >/dev/null

TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-install-test.XXXXXX")
RELEASE_DIR=$TEST_DIR/release
mkdir -p "$RELEASE_DIR"

cleanup() {
    if [ -d "$TEST_DIR" ]; then
        rm -rf "$TEST_DIR"
    fi
}
trap cleanup 0
trap 'exit 130' HUP INT TERM

for platform in \
    macos-arm64 \
    macos-x86_64 \
    linux-arm64 \
    linux-x86_64
do
    version=$(printf '%s' "$platform" | tr '_' '-')
    printf '#!/bin/sh\nprintf "me-s 1.0.0-%s\\n"\n' "$version" >"$RELEASE_DIR/me-s-$platform"
    printf '#!/bin/sh\nprintf "me-gateway 1.0.0-%s\\n"\n' "$version" >"$RELEASE_DIR/me-gateway-$platform"
    chmod 755 "$RELEASE_DIR/me-s-$platform" "$RELEASE_DIR/me-gateway-$platform"
done

(
    cd "$RELEASE_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum me-s-* me-gateway-* >SHA256SUMS
    else
        shasum -a 256 me-s-* me-gateway-* >SHA256SUMS
    fi
)

run_case() {
    os=$1
    arch=$2
    expected=$(printf '%s' "$3" | tr '_' '-')
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
    printf '%s\n' "$output" | grep -F "me-s 1.0.0-$expected" >/dev/null
    printf '%s\n' "$output" | grep -F "me-gateway 1.0.0-$expected" >/dev/null
    "$install_dir/me-s" version | grep -F "me-s 1.0.0-$expected" >/dev/null
    "$install_dir/me-gateway" version | grep -F "me-gateway 1.0.0-$expected" >/dev/null
    "$install_dir/me" | grep -F 'legacy me' >/dev/null
}

run_case Darwin arm64 macos-arm64
run_case Darwin x86_64 macos-x86_64
run_case Linux aarch64 linux-arm64
run_case Linux amd64 linux-x86_64

bad_release=$TEST_DIR/bad-release
bad_install=$TEST_DIR/bad-install
mkdir -p "$bad_release" "$bad_install"
cp "$RELEASE_DIR/me-s-linux-x86_64" "$bad_release/me-s-linux-x86_64"
cp "$RELEASE_DIR/me-gateway-linux-x86_64" "$bad_release/me-gateway-linux-x86_64"
(
    cd "$bad_release"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum me-gateway-linux-x86_64 >SHA256SUMS
    else
        shasum -a 256 me-gateway-linux-x86_64 >SHA256SUMS
    fi
    printf '%064d  me-s-linux-x86_64\n' 0 >>SHA256SUMS
)
printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$bad_install/me"
printf '#!/bin/sh\nprintf "old me-s\\n"\n' >"$bad_install/me-s"
printf '#!/bin/sh\nprintf "old gateway\\n"\n' >"$bad_install/me-gateway"
chmod 755 "$bad_install/me" "$bad_install/me-s" "$bad_install/me-gateway"
if ME_INSTALL_OS=Linux \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$bad_release" \
    ME_INSTALL_DIR="$bad_install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'checksum failure unexpectedly installed ME\n' >&2
    exit 1
fi
"$bad_install/me" | grep -F 'legacy me' >/dev/null
"$bad_install/me-s" | grep -F 'old me-s' >/dev/null
"$bad_install/me-gateway" | grep -F 'old gateway' >/dev/null

mismatch_release=$TEST_DIR/mismatch-release
mismatch_install=$TEST_DIR/mismatch-install
mkdir -p "$mismatch_release" "$mismatch_install"
cp "$RELEASE_DIR/me-s-linux-x86_64" "$mismatch_release/me-s-linux-x86_64"
printf '#!/bin/sh\nprintf "me-gateway 1.0.1-linux-x86-64\\n"\n' >"$mismatch_release/me-gateway-linux-x86_64"
chmod 755 "$mismatch_release/me-gateway-linux-x86_64"
(
    cd "$mismatch_release"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum me-s-linux-x86_64 me-gateway-linux-x86_64 >SHA256SUMS
    else
        shasum -a 256 me-s-linux-x86_64 me-gateway-linux-x86_64 >SHA256SUMS
    fi
)
printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$mismatch_install/me"
printf '#!/bin/sh\nprintf "old me-s\\n"\n' >"$mismatch_install/me-s"
printf '#!/bin/sh\nprintf "old gateway\\n"\n' >"$mismatch_install/me-gateway"
chmod 755 "$mismatch_install/me" "$mismatch_install/me-s" "$mismatch_install/me-gateway"
if ME_INSTALL_OS=Linux ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$mismatch_release" ME_INSTALL_DIR="$mismatch_install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'mismatched product versions unexpectedly installed ME\n' >&2
    exit 1
fi
"$mismatch_install/me" | grep -F 'legacy me' >/dev/null
"$mismatch_install/me-s" | grep -F 'old me-s' >/dev/null
"$mismatch_install/me-gateway" | grep -F 'old gateway' >/dev/null

rollback_install=$TEST_DIR/rollback-install
fake_bin=$TEST_DIR/fake-bin
mkdir -p "$rollback_install" "$fake_bin"
printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$rollback_install/me"
printf '#!/bin/sh\nprintf "old me-s\\n"\n' >"$rollback_install/me-s"
printf '#!/bin/sh\nprintf "old gateway\\n"\n' >"$rollback_install/me-gateway"
chmod 755 "$rollback_install/me" "$rollback_install/me-s" "$rollback_install/me-gateway"
real_mv=$(command -v mv)
cat >"$fake_bin/mv" <<EOF
#!/bin/sh
if [ "\${1:-}" = "-f" ]; then source=\${2:-}; else source=\${1:-}; fi
case "\$source" in
    */me-gateway.new) exit 1 ;;
esac
exec "$real_mv" "\$@"
EOF
chmod 755 "$fake_bin/mv"
if PATH="$fake_bin:$PATH" \
    ME_INSTALL_OS=Linux \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$RELEASE_DIR" \
    ME_INSTALL_DIR="$rollback_install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'second-program commit failure unexpectedly installed ME\n' >&2
    exit 1
fi
"$rollback_install/me" | grep -F 'legacy me' >/dev/null
"$rollback_install/me-s" | grep -F 'old me-s' >/dev/null
"$rollback_install/me-gateway" | grep -F 'old gateway' >/dev/null
if find "$rollback_install" -name '.me-install.*' -print | grep . >/dev/null; then
    printf 'failed installation left transaction artifacts\n' >&2
    exit 1
fi

if ME_INSTALL_OS=Plan9 \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$RELEASE_DIR" \
    ME_INSTALL_DIR="$TEST_DIR/unsupported" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'unsupported platform unexpectedly installed ME\n' >&2
    exit 1
fi

printf 'install.sh integration tests: PASS\n'
