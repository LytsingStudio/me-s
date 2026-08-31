#!/bin/sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
RELEASE=$ROOT_DIR/release.sh
UPDATER=$ROOT_DIR/src/updater.rs
LINUX_BUILDER=$ROOT_DIR/packaging/linux/build-run.sh
LINUX_CONTAINER=$ROOT_DIR/packaging/linux/build-container.sh
WINDOWS_BUILDER=$ROOT_DIR/packaging/windows/build-installer.sh
VERIFIER=$ROOT_DIR/scripts/verify-release-artifacts.sh

sh -n "$ROOT_DIR/install.sh"
sh -n "$ROOT_DIR/packaging/linux/installer.sh"
sh -n "$LINUX_BUILDER"
sh -n "$ROOT_DIR/packaging/macos/build-pkg.sh"
bash -n "$RELEASE" "$LINUX_CONTAINER" "$WINDOWS_BUILDER" "$VERIFIER"
node "$ROOT_DIR/scripts/product-version.cjs" --print >/dev/null

grep -F 'LytsingStudio/me-s' "$ROOT_DIR/install.sh" >/dev/null
if grep -F 'LytsingStudio/me-rust' "$ROOT_DIR/install.sh" >/dev/null; then
    printf 'install.sh still targets the legacy me-rust release repository\n' >&2
    exit 1
fi
if [ -e "$ROOT_DIR/.github/workflows/release.yml" ]; then
    printf 'local release construction must not retain a GitHub Actions workflow\n' >&2
    exit 1
fi

for asset in \
    ME-macos-universal.pkg \
    ME-windows-x86_64-setup.exe \
    ME-linux-x86_64.run \
    ME-linux-arm64.run
do
    grep -F "$asset" "$RELEASE" >/dev/null
    grep -F "$asset" "$UPDATER" >/dev/null
    grep -F "$asset" "$VERIFIER" >/dev/null
done
grep -F 'ME-macos-universal.pkg' "$ROOT_DIR/install.sh" >/dev/null
grep -F 'ME-linux-x86_64.run' "$ROOT_DIR/install.sh" >/dev/null
grep -F 'ME-linux-arm64.run' "$ROOT_DIR/install.sh" >/dev/null
grep -F 'ME-windows-x86_64-setup.exe' "$ROOT_DIR/install.ps1" >/dev/null
grep -F 'cargo xwin build' "$RELEASE" >/dev/null
grep -F 'packaging/linux/build-container.sh' "$RELEASE" >/dev/null
grep -F 'scripts/verify-release-artifacts.sh' "$RELEASE" >/dev/null
grep -F 'docker buildx build' "$LINUX_CONTAINER" >/dev/null
grep -F 'cargo zigbuild' "$ROOT_DIR/packaging/linux/Dockerfile" >/dev/null
grep -F 'makensis' "$WINDOWS_BUILDER" >/dev/null
grep -F 'gh release create' "$RELEASE" >/dev/null
grep -F 'RELEASE_BUILDER_NAME=me-s-release' "$RELEASE" >/dev/null
grep -F '        --driver docker-container \' "$RELEASE" >/dev/null
grep -F 'docker buildx rm --force "$RELEASE_BUILDER_NAME"' "$RELEASE" >/dev/null
grep -F 'trap cleanup_release_builder_on_exit EXIT' "$RELEASE" >/dev/null
grep -F 'export ME_RELEASE_BUILDER="$RELEASE_BUILDER_NAME"' "$RELEASE" >/dev/null
grep -F 'BUILDER_ARGS=(--builder "$ME_RELEASE_BUILDER")' "$LINUX_CONTAINER" >/dev/null
grep -F 'docker buildx build "${BUILDER_ARGS[@]}" \' "$LINUX_CONTAINER" >/dev/null
grep -F '            cd /' "$RELEASE" >/dev/null
grep -F 'colima ssh -- sudo fstrim -v /var/lib/docker' "$RELEASE" >/dev/null
if grep -F 'buildx prune' "$RELEASE" >/dev/null; then
    printf 'release must not rely on shared BuildKit cache pruning\n' >&2
    exit 1
fi
if grep -E 'gh (workflow|run)|GitHub Actions|release\.yml' "$RELEASE" "$LINUX_CONTAINER" "$VERIFIER" >/dev/null; then
    printf 'local release sources still depend on GitHub Actions\n' >&2
    exit 1
fi

if grep -E 'me-(s|gateway)-(macos|linux|windows)' \
    "$ROOT_DIR/install.sh" "$ROOT_DIR/install.ps1" "$RELEASE" "$UPDATER" >/dev/null
then
    printf 'release, install, or update sources still reference raw program assets\n' >&2
    exit 1
fi
if grep -E 'install\.sh.*#|install\.ps1.*#' "$RELEASE" >/dev/null; then
    printf 'bootstrap scripts must not be uploaded as Release assets\n' >&2
    exit 1
fi
if grep -E '"\$OUTPUT"[[:space:]]+--(version|extract-dir)' "$LINUX_BUILDER" >/dev/null; then
    printf 'Linux package construction must not execute the generated run package\n' >&2
    exit 1
fi

TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/me-install-test.XXXXXX")
RELEASE_DIR=$TEST_DIR/release
MOCK_DIR=$TEST_DIR/mock
mkdir -p "$RELEASE_DIR" "$MOCK_DIR/x86_64" "$MOCK_DIR/arm64"

cleanup() {
    if [ -d "$TEST_DIR" ]; then
        rm -rf "$TEST_DIR"
    fi
}
trap cleanup 0
trap 'exit 130' HUP INT TERM

VERSION=1.2.3

write_cli() {
    path=$1
    name=$2
    marker=$3
    cat >"$path" <<EOF
#!/bin/sh
case "\${1:-}" in
    version|--version) printf '$name $VERSION\\n' ;;
    identity) printf '$marker\\n' ;;
    *) printf '$name $VERSION\\n' ;;
esac
EOF
    chmod 755 "$path"
}

write_client() {
    path=$1
    marker=$2
    cat >"$path" <<EOF
#!/bin/sh
printf 'me-client $marker\\n'
EOF
    chmod 755 "$path"
}

for arch in x86_64 arm64; do
    write_cli "$MOCK_DIR/$arch/me-s" me-s "$arch-me-s"
    write_cli "$MOCK_DIR/$arch/me-gateway" me-gateway "$arch-me-gateway"
    write_client "$MOCK_DIR/$arch/me-client.AppImage" "$arch-client"
done

"$ROOT_DIR/packaging/linux/build-run.sh" \
    "$VERSION" x86_64 \
    "$MOCK_DIR/x86_64/me-s" \
    "$MOCK_DIR/x86_64/me-gateway" \
    "$MOCK_DIR/x86_64/me-client.AppImage" \
    "$RELEASE_DIR/ME-linux-x86_64.run" >/dev/null
"$ROOT_DIR/packaging/linux/build-run.sh" \
    "$VERSION" arm64 \
    "$MOCK_DIR/arm64/me-s" \
    "$MOCK_DIR/arm64/me-gateway" \
    "$MOCK_DIR/arm64/me-client.AppImage" \
    "$RELEASE_DIR/ME-linux-arm64.run" >/dev/null

checksum_files() {
    directory=$1
    shift
    (
        cd "$directory"
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$@"
        else
            shasum -a 256 "$@"
        fi
    )
}
checksum_files "$RELEASE_DIR" ME-linux-x86_64.run ME-linux-arm64.run >"$RELEASE_DIR/SHA256SUMS"

run_case() {
    arch=$1
    expected=$2
    install_dir=$TEST_DIR/install-$arch
    mkdir -p "$install_dir"
    printf '#!/bin/sh\nprintf "legacy me\\n"\n' >"$install_dir/me"
    printf 'keep\n' >"$install_dir/unrelated.txt"
    chmod 755 "$install_dir/me"

    output=$(
        ME_INSTALL_OS=Linux \
        ME_INSTALL_ARCH=$arch \
        ME_INSTALL_BASE_URL="file://$RELEASE_DIR" \
        ME_INSTALL_DIR="$install_dir" \
        sh "$ROOT_DIR/install.sh"
    )
    printf '%s\n' "$output" | grep -F "me-s $VERSION" >/dev/null
    printf '%s\n' "$output" | grep -F "me-gateway $VERSION" >/dev/null
    "$install_dir/me-s" version | grep -F "me-s $VERSION" >/dev/null
    "$install_dir/me-gateway" version | grep -F "me-gateway $VERSION" >/dev/null
    "$install_dir/me-client" | grep -F "me-client $expected-client" >/dev/null
    "$install_dir/me" | grep -F 'legacy me' >/dev/null
    grep -F keep "$install_dir/unrelated.txt" >/dev/null
}

run_case amd64 x86_64
run_case aarch64 arm64

bad_release=$TEST_DIR/bad-release
bad_install=$TEST_DIR/bad-install
mkdir -p "$bad_release" "$bad_install"
cp "$RELEASE_DIR/ME-linux-x86_64.run" "$bad_release/ME-linux-x86_64.run"
printf '%064d  ME-linux-x86_64.run\n' 0 >"$bad_release/SHA256SUMS"
for name in me-s me-gateway me-client; do
    printf '#!/bin/sh\nprintf "old %s\\n"\n' "$name" >"$bad_install/$name"
    chmod 755 "$bad_install/$name"
done
if ME_INSTALL_OS=Linux \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$bad_release" \
    ME_INSTALL_DIR="$bad_install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'checksum failure unexpectedly installed ME\n' >&2
    exit 1
fi
for name in me-s me-gateway me-client; do
    "$bad_install/$name" | grep -F "old $name" >/dev/null
done

duplicate_release=$TEST_DIR/duplicate-release
mkdir -p "$duplicate_release"
cp "$RELEASE_DIR/ME-linux-x86_64.run" "$duplicate_release/ME-linux-x86_64.run"
checksum_files "$duplicate_release" ME-linux-x86_64.run >"$duplicate_release/SHA256SUMS"
cat "$duplicate_release/SHA256SUMS" >>"$duplicate_release/SHA256SUMS.duplicate"
cat "$duplicate_release/SHA256SUMS" >>"$duplicate_release/SHA256SUMS.duplicate"
mv "$duplicate_release/SHA256SUMS.duplicate" "$duplicate_release/SHA256SUMS"
if ME_INSTALL_OS=Linux \
    ME_INSTALL_ARCH=x86_64 \
    ME_INSTALL_BASE_URL="file://$duplicate_release" \
    ME_INSTALL_DIR="$TEST_DIR/duplicate-install" \
    sh "$ROOT_DIR/install.sh" >/dev/null 2>&1
then
    printf 'duplicate checksum entries were accepted\n' >&2
    exit 1
fi

rollback_install=$TEST_DIR/rollback-install
fake_bin=$TEST_DIR/fake-bin
mkdir -p "$rollback_install" "$fake_bin"
for name in me-s me-gateway me-client; do
    printf '#!/bin/sh\nprintf "old %s\\n"\n' "$name" >"$rollback_install/$name"
    chmod 755 "$rollback_install/$name"
done
printf 'preserve\n' >"$rollback_install/unrelated.txt"
real_mv=$(command -v mv)
cat >"$fake_bin/mv" <<EOF
#!/bin/sh
case "\${1:-}" in
    */new/me-client) exit 1 ;;
esac
exec "$real_mv" "\$@"
EOF
chmod 755 "$fake_bin/mv"
if PATH="$fake_bin:$PATH" \
    "$RELEASE_DIR/ME-linux-x86_64.run" --install-dir "$rollback_install" >/dev/null 2>&1
then
    printf 'third-program commit failure unexpectedly installed ME\n' >&2
    exit 1
fi
for name in me-s me-gateway me-client; do
    "$rollback_install/$name" | grep -F "old $name" >/dev/null
done
grep -F preserve "$rollback_install/unrelated.txt" >/dev/null
if find "$rollback_install" -name '.me-product-install-*' -print | grep . >/dev/null; then
    printf 'failed product installation left transaction artifacts\n' >&2
    exit 1
fi

backup_install=$TEST_DIR/backup-rollback-install
mkdir -p "$backup_install"
for name in me-s me-gateway me-client; do
    printf '#!/bin/sh\nprintf "old %s\\n"\n' "$name" >"$backup_install/$name"
    chmod 755 "$backup_install/$name"
done
cat >"$fake_bin/mv" <<EOF
#!/bin/sh
case "\${1:-}" in
    "$backup_install/me-gateway") exit 1 ;;
esac
exec "$real_mv" "\$@"
EOF
chmod 755 "$fake_bin/mv"
if PATH="$fake_bin:$PATH" \
    "$RELEASE_DIR/ME-linux-x86_64.run" --install-dir "$backup_install" >/dev/null 2>&1
then
    printf 'second-program backup failure unexpectedly installed ME\n' >&2
    exit 1
fi
for name in me-s me-gateway me-client; do
    "$backup_install/$name" | grep -F "old $name" >/dev/null
done
if find "$backup_install" -name '.me-product-install-*' -print | grep . >/dev/null; then
    printf 'failed backup left transaction artifacts\n' >&2
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

printf 'complete product installation tests: PASS\n'