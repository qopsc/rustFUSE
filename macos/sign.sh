#!/bin/bash
# Developer ID signing and notarization for releases (macos/README.md,
# "Signing and releasing"). Runs in CI on a Mac with these variables, all
# from repository secrets:
#
#   APPLE_TEAM_ID                     team identifier, e.g. ABCDE12345
#   APPLE_BUNDLE_PREFIX               optional, default dev.rustfuse
#   APPLE_DEVELOPER_ID_P12            base64 of the "Developer ID Application"
#                                     certificate and its key (.p12)
#   APPLE_DEVELOPER_ID_P12_PASSWORD   the .p12's password
#   APPLE_PROFILE_APP                 base64 of the Developer ID provisioning
#   APPLE_PROFILE_FILEPROVIDER        profiles for <prefix>.app,
#   APPLE_PROFILE_FS                  <prefix>.app.fileprovider, <prefix>.app.fs
#   APPLE_API_KEY_ID                  App Store Connect API key for notarytool
#   APPLE_API_ISSUER_ID
#   APPLE_API_KEY_P8                  base64 of the key's .p8 file
#
#   sign.sh available            exits 0 when the secrets are set, 1 when
#                                none are, 3 when only some are
#   sign.sh cli <file>...        signs command line tools and notarizes them
#   sign.sh app <out-dir>        builds, signs, notarizes and staples the app;
#                                writes <out-dir>/rustFUSE-<version>.zip
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=${RUNNER_TEMP:-$(mktemp -d)}/rustfuse-signing
keychain=$work/signing.keychain-db
prefix=${APPLE_BUNDLE_PREFIX:-dev.rustfuse}

required=(APPLE_TEAM_ID APPLE_DEVELOPER_ID_P12 APPLE_DEVELOPER_ID_P12_PASSWORD
  APPLE_PROFILE_APP APPLE_PROFILE_FILEPROVIDER APPLE_PROFILE_FS
  APPLE_API_KEY_ID APPLE_API_ISSUER_ID APPLE_API_KEY_P8)

available() {
  local missing=()
  for v in "${required[@]}"; do
    [ -n "${!v:-}" ] || missing+=("$v")
  done
  if [ ${#missing[@]} -eq ${#required[@]} ]; then
    return 1
  fi
  if [ ${#missing[@]} -gt 0 ]; then
    echo "signing secrets missing: ${missing[*]}" >&2
    exit 3
  fi
}

# A keychain of its own with the certificate, the profiles where Xcode
# looks for them, and the notary key. Done once per job.
setup() {
  [ -f "$work/ready" ] && return
  mkdir -p "$work"
  echo "$APPLE_DEVELOPER_ID_P12" | base64 --decode > "$work/cert.p12"
  local pass
  pass=$(uuidgen)
  security create-keychain -p "$pass" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$pass" "$keychain"
  security import "$work/cert.p12" -k "$keychain" -P "$APPLE_DEVELOPER_ID_P12_PASSWORD" \
    -T /usr/bin/codesign -T /usr/bin/security
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pass" "$keychain" >/dev/null
  # shellcheck disable=SC2046
  security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
  rm "$work/cert.p12"
  for dir in "$HOME/Library/MobileDevice/Provisioning Profiles" \
    "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"; do
    mkdir -p "$dir"
    for p in APP FILEPROVIDER FS; do
      local var=APPLE_PROFILE_$p
      echo "${!var}" | base64 --decode > "$dir/rustfuse-$p.provisionprofile"
    done
  done
  echo "$APPLE_API_KEY_P8" | base64 --decode > "$work/notary.p8"
  touch "$work/ready"
}

identity() {
  security find-identity -v -p codesigning "$keychain" \
    | sed -n 's/.*"\(Developer ID Application: .*\)"$/\1/p' | head -1
}

profile_name() {
  local var=APPLE_PROFILE_$1
  echo "${!var}" | base64 --decode > "$work/profile"
  security cms -D -i "$work/profile" > "$work/profile.plist"
  /usr/libexec/PlistBuddy -c "Print :Name" "$work/profile.plist"
}

notarize() {
  xcrun notarytool submit "$1" --key "$work/notary.p8" --key-id "$APPLE_API_KEY_ID" \
    --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 1h --output-format json \
    | tee "$work/notary.json"
  if ! grep -q '"status" *: *"Accepted"' "$work/notary.json"; then
    id=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$work/notary.json" | head -1)
    [ -n "$id" ] && xcrun notarytool log "$id" --key "$work/notary.p8" \
      --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID" || true
    echo "notarization failed" >&2
    exit 1
  fi
}

cli() {
  setup
  local id
  id=$(identity)
  for f in "$@"; do
    codesign --force --options runtime --timestamp --sign "$id" "$f"
    codesign --verify --strict "$f"
  done
  # Bare tools cannot be stapled; Gatekeeper finds the ticket online.
  rm -f "$work/cli.zip"
  zip -j -q "$work/cli.zip" "$@"
  notarize "$work/cli.zip"
}

app() {
  setup
  local out=$1 version
  mkdir -p "$out"
  version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$here/../Cargo.toml" | head -1)
  cat > "$here/Config/Local.xcconfig" <<EOF
DEVELOPMENT_TEAM = $APPLE_TEAM_ID
RUSTFUSE_BUNDLE_PREFIX = $prefix
CODE_SIGN_STYLE = Manual
CODE_SIGN_IDENTITY = Developer ID Application
RUSTFUSE_PROFILE_APP = $(profile_name APP)
RUSTFUSE_PROFILE_FILEPROVIDER = $(profile_name FILEPROVIDER)
RUSTFUSE_PROFILE_FS = $(profile_name FS)
EOF
  (cd "$here" && xcodegen generate)
  xcodebuild -project "$here/rustFUSE.xcodeproj" -scheme rustFUSE -configuration Release \
    -archivePath "$work/rustFUSE.xcarchive" MARKETING_VERSION="$version" archive
  cat > "$work/export.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>developer-id</string>
  <key>signingStyle</key><string>manual</string>
  <key>teamID</key><string>$APPLE_TEAM_ID</string>
  <key>signingCertificate</key><string>Developer ID Application</string>
  <key>provisioningProfiles</key>
  <dict>
    <key>$prefix.app</key><string>$(profile_name APP)</string>
    <key>$prefix.app.fileprovider</key><string>$(profile_name FILEPROVIDER)</string>
    <key>$prefix.app.fs</key><string>$(profile_name FS)</string>
  </dict>
</dict>
</plist>
EOF
  xcodebuild -exportArchive -archivePath "$work/rustFUSE.xcarchive" \
    -exportOptionsPlist "$work/export.plist" -exportPath "$work/export"
  local bundle=$work/export/rustFUSE.app
  codesign --verify --deep --strict "$bundle"
  ditto -c -k --keepParent "$bundle" "$work/notarize.zip"
  notarize "$work/notarize.zip"
  xcrun stapler staple "$bundle"
  spctl --assess --type execute -v "$bundle"
  ditto -c -k --keepParent "$bundle" "$out/rustFUSE-$version.zip"
  (cd "$out" && shasum -a 256 "rustFUSE-$version.zip" > "rustFUSE-$version.zip.sha256")
}

case ${1:-} in
  available) available ;;
  cli) shift; available && cli "$@" ;;
  app) available && app "$2" ;;
  *) echo "usage: sign.sh available | cli <file>... | app <out-dir>" >&2; exit 2 ;;
esac
