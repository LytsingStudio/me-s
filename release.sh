#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$ROOT_DIR/dist"
MACOS_ARM64_TARGET="aarch64-apple-darwin"
MACOS_X86_64_TARGET="x86_64-apple-darwin"
LINUX_ARM64_TARGET="aarch64-unknown-linux-gnu"
LINUX_X86_64_TARGET="x86_64-unknown-linux-gnu"
WINDOWS_TARGET="x86_64-pc-windows-gnu"
HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
VERSION="$(awk -F'"' '/^version = "/ { print $2; exit }' "$ROOT_DIR/Cargo.toml")"
TAG="v$VERSION"
EXPECTED_REPOSITORY="${ME_RELEASE_REPOSITORY:-LytsingStudio/me-s}"
EXPECTED_BRANCH="${ME_RELEASE_BRANCH:-s}"

MACOS_ARM64_ME_S="me-s-macos-arm64"
MACOS_ARM64_GATEWAY="me-gateway-macos-arm64"
MACOS_X86_64_ME_S="me-s-macos-x86_64"
MACOS_X86_64_GATEWAY="me-gateway-macos-x86_64"
LINUX_ARM64_ME_S="me-s-linux-arm64"
LINUX_ARM64_GATEWAY="me-gateway-linux-arm64"
LINUX_X86_64_ME_S="me-s-linux-x86_64"
LINUX_X86_64_GATEWAY="me-gateway-linux-x86_64"
WINDOWS_X86_64_ME_S="me-s-windows-x86_64.exe"
WINDOWS_X86_64_GATEWAY="me-gateway-windows-x86_64.exe"

case "$HOST_TARGET" in
    aarch64-apple-darwin|x86_64-apple-darwin) ;;
    *)
        echo "error: release.sh must run on macOS; current Rust host is $HOST_TARGET" >&2
        exit 1
        ;;
esac

for command in \
    cargo \
    cargo-zigbuild \
    clang \
    codesign \
    curl \
    file \
    gh \
    git \
    rustc \
    rustup \
    shasum \
    strings \
    x86_64-w64-mingw32-gcc \
    x86_64-w64-mingw32-objdump \
    zig
do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "error: missing build dependency: $command" >&2
        echo "install on macOS with: brew install gh zig cargo-zigbuild mingw-w64" >&2
        exit 1
    fi
done

cd "$ROOT_DIR"

sh -n "$ROOT_DIR/install.sh"
"$ROOT_DIR/tests/install_scripts.sh"
grep -F 'me-gateway-windows-x86_64.exe' "$ROOT_DIR/install.ps1" >/dev/null
grep -F '$installed = @($false, $false)' "$ROOT_DIR/install.ps1" >/dev/null

BUILD_CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
REMAP_SEPARATOR=$'\x1f'
REMAP_FLAGS="--remap-path-prefix=$ROOT_DIR=/source${REMAP_SEPARATOR}--remap-path-prefix=$BUILD_CARGO_HOME=/cargo${REMAP_SEPARATOR}--remap-path-prefix=$HOME=/home"
if [[ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
    export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}${REMAP_SEPARATOR}${REMAP_FLAGS}"
else
    export CARGO_ENCODED_RUSTFLAGS="$REMAP_FLAGS"
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    echo "error: Cargo.toml has invalid release version: $VERSION" >&2
    exit 1
fi
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    echo "error: release requires a clean Git worktree" >&2
    exit 1
fi
BRANCH="$(git branch --show-current)"
if [[ -z "$BRANCH" ]]; then
    echo "error: release cannot run from a detached HEAD" >&2
    exit 1
fi
if [[ "$BRANCH" != "$EXPECTED_BRANCH" ]]; then
    echo "error: release must run from $EXPECTED_BRANCH; current branch is $BRANCH" >&2
    exit 1
fi
if ! git remote get-url origin >/dev/null 2>&1; then
    echo "error: release requires an origin remote" >&2
    exit 1
fi
if ! gh auth status --hostname github.com >/dev/null 2>&1; then
    echo "error: GitHub CLI is not logged in; run gh auth login" >&2
    exit 1
fi

REPOSITORY="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
if [[ "$REPOSITORY" != "$EXPECTED_REPOSITORY" ]]; then
    echo "error: release repository must be $EXPECTED_REPOSITORY; current origin resolves to $REPOSITORY" >&2
    exit 1
fi
git fetch --quiet origin "$BRANCH" --tags
HEAD_COMMIT="$(git rev-parse HEAD)"
REMOTE_COMMIT="$(git rev-parse "origin/$BRANCH")"
if [[ "$HEAD_COMMIT" != "$REMOTE_COMMIT" ]]; then
    echo "error: local HEAD is not the commit currently pushed to origin/$BRANCH" >&2
    exit 1
fi
if gh release view "$TAG" --repo "$REPOSITORY" >/dev/null 2>&1; then
    echo "error: GitHub Release $TAG already exists in $REPOSITORY" >&2
    exit 1
fi
if git rev-parse --quiet --verify "refs/tags/$TAG" >/dev/null; then
    TAG_COMMIT="$(git rev-list -n 1 "$TAG")"
    if [[ "$TAG_COMMIT" != "$HEAD_COMMIT" ]]; then
        echo "error: existing tag $TAG does not point to HEAD" >&2
        exit 1
    fi
    TAG_EXISTS=1
else
    TAG_EXISTS=0
fi

echo "publishing $REPOSITORY $TAG from $HEAD_COMMIT"
echo "cleaning previous build outputs"
cargo clean
rm -rf "$DIST_DIR"
rm -rf "$ROOT_DIR/.build/python"
mkdir -p "$DIST_DIR"

rustup target add \
    "$MACOS_ARM64_TARGET" \
    "$MACOS_X86_64_TARGET" \
    "$LINUX_ARM64_TARGET" \
    "$LINUX_X86_64_TARGET" \
    "$WINDOWS_TARGET"

copy_unix_products() {
    target=$1
    me_s_name=$2
    gateway_name=$3
    cp "target/$target/release/me-s" "$DIST_DIR/$me_s_name"
    cp "target/$target/release/me-gateway" "$DIST_DIR/$gateway_name"
    chmod 755 "$DIST_DIR/$me_s_name" "$DIST_DIR/$gateway_name"
}

copy_windows_products() {
    target=$1
    me_s_name=$2
    gateway_name=$3
    cp "target/$target/release/me-s.exe" "$DIST_DIR/$me_s_name"
    cp "target/$target/release/me-gateway.exe" "$DIST_DIR/$gateway_name"
    chmod 755 "$DIST_DIR/$me_s_name" "$DIST_DIR/$gateway_name"
}

echo "building macOS $MACOS_ARM64_TARGET"
cargo build --locked --release --bins --target "$MACOS_ARM64_TARGET"
copy_unix_products "$MACOS_ARM64_TARGET" "$MACOS_ARM64_ME_S" "$MACOS_ARM64_GATEWAY"

echo "building macOS $MACOS_X86_64_TARGET"
cargo build --locked --release --bins --target "$MACOS_X86_64_TARGET"
copy_unix_products "$MACOS_X86_64_TARGET" "$MACOS_X86_64_ME_S" "$MACOS_X86_64_GATEWAY"

echo "building Linux $LINUX_ARM64_TARGET (glibc 2.17)"
cargo zigbuild --locked --release --bins --target "$LINUX_ARM64_TARGET.2.17"
copy_unix_products "$LINUX_ARM64_TARGET" "$LINUX_ARM64_ME_S" "$LINUX_ARM64_GATEWAY"

echo "building Linux $LINUX_X86_64_TARGET (glibc 2.17)"
cargo zigbuild --locked --release --bins --target "$LINUX_X86_64_TARGET.2.17"
copy_unix_products "$LINUX_X86_64_TARGET" "$LINUX_X86_64_ME_S" "$LINUX_X86_64_GATEWAY"

echo "building Windows $WINDOWS_TARGET"
cargo build --locked --release --bins --target "$WINDOWS_TARGET"
copy_windows_products "$WINDOWS_TARGET" "$WINDOWS_X86_64_ME_S" "$WINDOWS_X86_64_GATEWAY"

cp "$ROOT_DIR/install.sh" "$DIST_DIR/install.sh"
cp "$ROOT_DIR/install.ps1" "$DIST_DIR/install.ps1"
chmod 755 "$DIST_DIR/install.sh"

file "$DIST_DIR/$MACOS_ARM64_ME_S" | grep -Eq "Mach-O 64-bit executable arm64"
file "$DIST_DIR/$MACOS_ARM64_GATEWAY" | grep -Eq "Mach-O 64-bit executable arm64"
file "$DIST_DIR/$MACOS_X86_64_ME_S" | grep -Eq "Mach-O 64-bit executable x86_64"
file "$DIST_DIR/$MACOS_X86_64_GATEWAY" | grep -Eq "Mach-O 64-bit executable x86_64"
file "$DIST_DIR/$LINUX_ARM64_ME_S" | grep -Eq "ELF 64-bit.*ARM aarch64"
file "$DIST_DIR/$LINUX_ARM64_GATEWAY" | grep -Eq "ELF 64-bit.*ARM aarch64"
file "$DIST_DIR/$LINUX_X86_64_ME_S" | grep -Eq "ELF 64-bit.*x86-64"
file "$DIST_DIR/$LINUX_X86_64_GATEWAY" | grep -Eq "ELF 64-bit.*x86-64"
file "$DIST_DIR/$WINDOWS_X86_64_ME_S" | grep -Eq "PE32\\+ executable.*x86-64"
file "$DIST_DIR/$WINDOWS_X86_64_GATEWAY" | grep -Eq "PE32\\+ executable.*x86-64"

strings "$DIST_DIR/$MACOS_ARM64_ME_S" \
    | grep -F "cpython-3.12.13+20260718-$MACOS_ARM64_TARGET" >/dev/null
strings "$DIST_DIR/$MACOS_X86_64_ME_S" \
    | grep -F "cpython-3.12.13+20260718-$MACOS_X86_64_TARGET" >/dev/null
strings "$DIST_DIR/$LINUX_ARM64_ME_S" \
    | grep -F "cpython-3.12.13+20260718-$LINUX_ARM64_TARGET" >/dev/null
strings "$DIST_DIR/$LINUX_X86_64_ME_S" \
    | grep -F "cpython-3.12.13+20260718-$LINUX_X86_64_TARGET" >/dev/null
strings "$DIST_DIR/$WINDOWS_X86_64_ME_S" \
    | grep -F "cpython-3.12.13+20260718-$WINDOWS_TARGET" >/dev/null

ARTIFACTS=(
    "$DIST_DIR/$MACOS_ARM64_ME_S"
    "$DIST_DIR/$MACOS_ARM64_GATEWAY"
    "$DIST_DIR/$MACOS_X86_64_ME_S"
    "$DIST_DIR/$MACOS_X86_64_GATEWAY"
    "$DIST_DIR/$LINUX_ARM64_ME_S"
    "$DIST_DIR/$LINUX_ARM64_GATEWAY"
    "$DIST_DIR/$LINUX_X86_64_ME_S"
    "$DIST_DIR/$LINUX_X86_64_GATEWAY"
    "$DIST_DIR/$WINDOWS_X86_64_ME_S"
    "$DIST_DIR/$WINDOWS_X86_64_GATEWAY"
)
for artifact in "${ARTIFACTS[@]}"; do
    for private_prefix in "$ROOT_DIR" "$BUILD_CARGO_HOME" "$HOME"; do
        if strings "$artifact" | grep -F "$private_prefix" >/dev/null; then
            echo "error: release artifact contains a private build path" >&2
            exit 1
        fi
    done
    if strings "$artifact" \
        | grep -E '(sk-[A-Za-z0-9_-]{16,}|github_pat_[A-Za-z0-9_]{20,}|ghp_[A-Za-z0-9_]{20,})' >/dev/null
    then
        echo "error: release artifact contains a credential-like string" >&2
        exit 1
    fi
done
for artifact in "$DIST_DIR/$WINDOWS_X86_64_ME_S" "$DIST_DIR/$WINDOWS_X86_64_GATEWAY"; do
    if x86_64-w64-mingw32-objdump -p "$artifact" \
        | grep -Eiq "DLL Name: (libgcc|libstdc\\+\\+|libwinpthread)"
    then
        echo "error: Windows artifact depends on an external MinGW runtime DLL" >&2
        exit 1
    fi
done

(
    cd "$DIST_DIR"
    shasum -a 256 \
        "$MACOS_ARM64_ME_S" \
        "$MACOS_ARM64_GATEWAY" \
        "$MACOS_X86_64_ME_S" \
        "$MACOS_X86_64_GATEWAY" \
        "$LINUX_ARM64_ME_S" \
        "$LINUX_ARM64_GATEWAY" \
        "$LINUX_X86_64_ME_S" \
        "$LINUX_X86_64_GATEWAY" \
        "$WINDOWS_X86_64_ME_S" \
        "$WINDOWS_X86_64_GATEWAY" \
        install.sh \
        install.ps1 \
        > SHA256SUMS
)

echo
echo "release artifacts:"
ls -lh "${ARTIFACTS[@]}" \
    "$DIST_DIR/install.sh" \
    "$DIST_DIR/install.ps1" \
    "$DIST_DIR/SHA256SUMS"

if [[ "$TAG_EXISTS" == 0 ]]; then
    git tag -a "$TAG" -m "ME $VERSION"
fi
git push origin "refs/tags/$TAG"

gh release create "$TAG" \
    "$DIST_DIR/$MACOS_ARM64_ME_S#$MACOS_ARM64_ME_S" \
    "$DIST_DIR/$MACOS_ARM64_GATEWAY#$MACOS_ARM64_GATEWAY" \
    "$DIST_DIR/$MACOS_X86_64_ME_S#$MACOS_X86_64_ME_S" \
    "$DIST_DIR/$MACOS_X86_64_GATEWAY#$MACOS_X86_64_GATEWAY" \
    "$DIST_DIR/$LINUX_ARM64_ME_S#$LINUX_ARM64_ME_S" \
    "$DIST_DIR/$LINUX_ARM64_GATEWAY#$LINUX_ARM64_GATEWAY" \
    "$DIST_DIR/$LINUX_X86_64_ME_S#$LINUX_X86_64_ME_S" \
    "$DIST_DIR/$LINUX_X86_64_GATEWAY#$LINUX_X86_64_GATEWAY" \
    "$DIST_DIR/$WINDOWS_X86_64_ME_S#$WINDOWS_X86_64_ME_S" \
    "$DIST_DIR/$WINDOWS_X86_64_GATEWAY#$WINDOWS_X86_64_GATEWAY" \
    "$DIST_DIR/install.sh#Unix installer" \
    "$DIST_DIR/install.ps1#Windows installer" \
    "$DIST_DIR/SHA256SUMS#SHA-256 checksums" \
    --repo "$REPOSITORY" \
    --verify-tag \
    --title "$TAG" \
    --generate-notes \
    --fail-on-no-commits \
    --latest

echo
echo "published release:"
gh release view "$TAG" --repo "$REPOSITORY" --json url --jq .url
