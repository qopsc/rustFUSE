#!/bin/sh
# Builds the rustfuse command line tool and puts it in the app bundle
# (Contents/Helpers/rustfuse), signed like the app when the app is signed.
# Where macFUSE is installed, also bundles a build with macFUSE support
# (Contents/Helpers/rustfuse-macfuse) for mount mode; it only starts where
# macFUSE is installed, so the plain build stays the default.
set -eu
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
cd "$SRCROOT/.."
profile=debug
flags=""
if [ "$CONFIGURATION" = Release ]; then
  profile=release
  flags="--release"
fi
helpers="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
mkdir -p "$helpers"
# shellcheck disable=SC2086
cargo build $flags -p rustfuse
cp "target/$profile/rustfuse" "$helpers/rustfuse"
rm -f "$helpers/rustfuse-macfuse"
if [ -d /Library/Filesystems/macfuse.fs ] && pkg-config --exists fuse 2>/dev/null; then
  # Its own target directory, so the two builds don't rebuild each other.
  # shellcheck disable=SC2086
  cargo build $flags -p rustfuse --features macfuse --target-dir target/macfuse
  cp "target/macfuse/$profile/rustfuse" "$helpers/rustfuse-macfuse"
else
  echo "macFUSE not installed: building without mount mode" >&2
fi
if [ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ] && [ "${CODE_SIGNING_ALLOWED:-YES}" = YES ]; then
  # Release builds are notarized, which needs a secure timestamp.
  timestamp=--timestamp=none
  [ "$CONFIGURATION" = Release ] && timestamp=--timestamp
  for tool in "$helpers"/*; do
    codesign --force --options runtime "$timestamp" \
      --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$tool"
  done
fi
