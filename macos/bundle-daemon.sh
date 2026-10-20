#!/bin/sh
# Builds the rustfuse command line tool and puts it in the app bundle
# (Contents/Helpers/rustfuse), signed like the app when the app is signed.
set -eu
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
cd "$SRCROOT/.."
if [ "$CONFIGURATION" = Release ]; then
  cargo build --release -p rustfuse
  bin=target/release/rustfuse
else
  cargo build -p rustfuse
  bin=target/debug/rustfuse
fi
helpers="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
mkdir -p "$helpers"
cp "$bin" "$helpers/rustfuse"
if [ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ] && [ "${CODE_SIGNING_ALLOWED:-YES}" = YES ]; then
  codesign --force --options runtime --timestamp=none \
    --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$helpers/rustfuse"
fi
