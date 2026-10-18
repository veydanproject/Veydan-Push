#!/usr/bin/env bash
# Builds the release binary: one static file that runs on any Linux server.
#
#   dist/vpush          the binary
#   dist/vpush.sha256   its checksum
#   dist/RELEASE        the release name, <version>-<commit>
#   dist/vguard         the read policy of a relay (crates/vguard), the same way
#   dist/vguard.sha256  its checksum; dist/vguard.RELEASE its release name
#
# A static build needs a C compiler for musl. Order of choice: zig (fetched
# into the tools directory when missing), then musl-gcc from the system.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/env.sh"
cd "$VPUSH_DIR"

# The monorepo's build environment points C compilers at the desktop's
# glibc headers (CFLAGS) and the linker at its libraries (RUSTFLAGS). A
# static musl build must see none of that: C compiled against glibc 2.38+
# headers wants __isoc23_sscanf and open64, which musl has not.
unset CFLAGS CXXFLAGS CPPFLAGS RUSTFLAGS GCC_EXEC_PREFIX

TARGET="${VPUSH_TARGET:-x86_64-unknown-linux-musl}"
ZIG_VERSION="${VPUSH_ZIG_VERSION:-0.13.0}"
ZIG_DIR="$VPUSH_TOOLS/zig"

say() { echo ">> $*"; }
die() { echo "vpush build: $*" >&2; exit 1; }

# --- what is being built -----------------------------------------------------
sha="$(git -C "$VPUSH_DIR" rev-parse --short=9 HEAD 2>/dev/null || echo unknown)"
if [ "$sha" != unknown ] && [ -n "$(git -C "$VPUSH_DIR" status --porcelain -- . 2>/dev/null)" ]; then
  # Uncommitted changes: two such builds differ, so the name must differ too.
  sha="$sha-dirty$(date -u +%H%M%S)"
fi
export VPUSH_GIT_SHA="$sha"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(date +%s)}"

# --- toolchain ---------------------------------------------------------------
if command -v rustup >/dev/null 2>&1; then
  rustup target list --installed | grep -qx "$TARGET" || {
    say "adding rust target $TARGET"
    rustup target add "$TARGET" >/dev/null
  }
fi

fetch_zig() {
  [ "${VPUSH_NO_FETCH:-0}" = 1 ] && return 1
  local arch; arch="$(uname -m)"
  local name="zig-linux-$arch-$ZIG_VERSION"
  say "fetching zig $ZIG_VERSION into $ZIG_DIR"
  mkdir -p "$ZIG_DIR"
  curl -fsSL "https://ziglang.org/download/$ZIG_VERSION/$name.tar.xz" \
    | tar -xJ -C "$ZIG_DIR" --strip-components=1
}

[ -x "$ZIG_DIR/zig" ] && export PATH="$ZIG_DIR:$PATH"

builder=""
if command -v zig >/dev/null 2>&1 || fetch_zig; then
  export PATH="$ZIG_DIR:$PATH"
  if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    say "installing cargo-zigbuild"
    cargo install cargo-zigbuild --locked >/dev/null 2>&1 || die "cannot install cargo-zigbuild"
  fi
  builder="zigbuild"
elif command -v musl-gcc >/dev/null 2>&1; then
  builder="build"
else
  die "no C compiler for $TARGET: install zig (https://ziglang.org) or musl-tools"
fi

# --- build -------------------------------------------------------------------
say "building vpush and vguard $sha for $TARGET (cargo $builder)"
cargo "$builder" --release --locked --target "$TARGET" -p vpush -p vguard

bin="$(cargo_target_dir)/$TARGET/release/vpush"
guard="$(cargo_target_dir)/$TARGET/release/vguard"
for b in "$bin" "$guard"; do
  [ -x "$b" ] || die "no binary at $b"
  # A binary that needs libraries from the build machine is not a release.
  if command -v ldd >/dev/null 2>&1 && ldd "$b" 2>/dev/null | grep -q '=>'; then
    die "$b is dynamically linked"
  fi
done

mkdir -p dist
cp "$bin" dist/vpush
(cd dist && sha256sum vpush > vpush.sha256)
cp "$guard" dist/vguard
(cd dist && sha256sum vguard > vguard.sha256)
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
echo "$version-$sha" > dist/RELEASE
echo "$version-$sha" > dist/vguard.RELEASE

say "dist/vpush  $(cat dist/RELEASE)  $(du -h dist/vpush | cut -f1)"
say "dist/vguard  $(cat dist/vguard.RELEASE)  $(du -h dist/vguard | cut -f1)"
