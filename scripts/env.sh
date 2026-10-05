#!/usr/bin/env bash
# Build environment of VPush — meant to be *sourced*.
#
# Inside the Veydan monorepo (this folder is services/push there) the
# project-local toolchain is used; VEYDAN_TOOLCHAINS says where it is, the
# one place that knows. Anywhere else, whatever cargo is on PATH.

VPUSH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export VPUSH_DIR

VEYDAN_TOOLCHAINS="${VEYDAN_TOOLCHAINS:-$VPUSH_DIR/../../data/toolchains}"
if [ -x "$VEYDAN_TOOLCHAINS/cargo/bin/cargo" ]; then
  VPUSH_TOOLS="$(cd "$VEYDAN_TOOLCHAINS" && pwd)"
  export RUSTUP_HOME="$VPUSH_TOOLS/rustup"
  export CARGO_HOME="$VPUSH_TOOLS/cargo"
  export PATH="$CARGO_HOME/bin:$PATH"
else
  # Tools VPush fetches for itself (zig) go here when it lives alone.
  VPUSH_TOOLS="$VPUSH_DIR/.tools"
fi
export VPUSH_TOOLS

if ! command -v cargo >/dev/null 2>&1; then
  echo "vpush: cargo not found; install Rust from https://rustup.rs" >&2
  return 1 2>/dev/null || exit 1
fi

# The folder cargo builds into, as cargo resolves it: target/ of this folder
# when it stands alone, the monorepo's data/target inside the monorepo (its
# .cargo/config.toml), CARGO_TARGET_DIR when that is set.
cargo_target_dir() {
  cargo metadata --format-version 1 --no-deps --manifest-path "$VPUSH_DIR/Cargo.toml" \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'
}
