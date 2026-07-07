#!/usr/bin/env bash
# build-adhoc.sh — build an Ad Hoc–signed .ipa for direct over-the-air install on registered
# devices (no TestFlight, no beta review, no code). Headless from the Mac Mini.
#
# Prereqs (one-time, already done 2026-07-01):
#   - Device UDID registered on the team (dev-signing auto-registers it).
#   - Ad Hoc provisioning profile "nimmesh Ad Hoc" created via the ASC API (includes the
#     distribution cert + the registered devices) and installed into
#     ~/Library/MobileDevice/Provisioning Profiles/<uuid>.mobileprovision
#   - Same testflight.env + nimmesh-build keychain as build-testflight.sh.
#
# USAGE:
#   apple/scripts/build-adhoc.sh              # framework + archive + export
#   apple/scripts/build-adhoc.sh --skip-rust  # skip the cargo-swift framework rebuild
#
# Output: build/adhoc/export/NimmeshApp.ipa — host it + a manifest.plist over HTTPS and
# install from Safari via itms-services:// (see ota/index.html). Distribution-signed, so
# no "trust developer" step is needed on the phone.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
APPLE="$REPO/apple"
BUILD="$REPO/build/adhoc"
ARCHIVE="$BUILD/NimmeshApp.xcarchive"
EXPORT_DIR="$BUILD/export"
ADHOC_PROFILE_NAME="nimmesh Ad Hoc"

SKIP_RUST=0
for a in "$@"; do
  case "$a" in
    --skip-rust) SKIP_RUST=1 ;;
    *) echo "unknown arg: $a (use --skip-rust)"; exit 2 ;;
  esac
done

CFG="$HOME/secrets/testflight.env"
[ -f "$CFG" ] || { echo "ERROR: missing $CFG (see build-testflight.sh header)"; exit 1; }
set -a; . "$CFG"; set +a
: "${ASC_TEAM_ID:?set ASC_TEAM_ID in $CFG}"
: "${ASC_SIGN_IDENTITY:?set ASC_SIGN_IDENTITY in $CFG}"

if [ -n "${ASC_KEYCHAIN:-}" ] && [ -f "$ASC_KEYCHAIN" ]; then
  security unlock-keychain -p "${ASC_KEYCHAIN_PASS:-}" "$ASC_KEYCHAIN" 2>/dev/null || true
  security list-keychains -d user | tr -d '"' | grep -qF "$ASC_KEYCHAIN" \
    || security list-keychains -d user -s "$ASC_KEYCHAIN" $(security list-keychains -d user | sed 's/"//g')
fi

source "$HOME/.cargo/env"

MARKETING_VERSION="$(grep -m1 '^version' "$REPO/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
BUILD_NUMBER="$(date +%Y%m%d%H%M)"
echo "==> nimmesh $MARKETING_VERSION (build $BUILD_NUMBER) -> Ad Hoc ipa (team $ASC_TEAM_ID)"

if [ "$SKIP_RUST" -eq 0 ]; then
  echo "==> cargo swift package (iOS framework)"
  ( cd "$REPO/crates/nimmesh-core" && cargo swift package -n NimmeshCore -p ios -y -F gateway-rpc >/dev/null )
fi
echo "==> xcodegen generate"
( cd "$APPLE" && xcodegen generate >/dev/null )

rm -rf "$BUILD"; mkdir -p "$BUILD"
echo "==> archiving (this is the slow step)"
xcodebuild archive \
  -project "$APPLE/NimmeshApp.xcodeproj" \
  -scheme NimmeshApp \
  -configuration Release \
  -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -archivePath "$ARCHIVE" \
  DEVELOPMENT_TEAM="$ASC_TEAM_ID" \
  CODE_SIGN_STYLE=Manual \
  CODE_SIGN_IDENTITY="$ASC_SIGN_IDENTITY" \
  PROVISIONING_PROFILE_SPECIFIER="$ADHOC_PROFILE_NAME" \
  OTHER_CODE_SIGN_FLAGS="--keychain $ASC_KEYCHAIN" \
  MARKETING_VERSION="$MARKETING_VERSION" \
  CURRENT_PROJECT_VERSION="$BUILD_NUMBER" \
  | grep -E "Archive (succeeded|failed)|error:|\*\* ARCHIVE" || true
[ -d "$ARCHIVE" ] || { echo "ERROR: archive not produced"; exit 1; }

cat > "$BUILD/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>ad-hoc</string>
  <key>teamID</key><string>$ASC_TEAM_ID</string>
  <key>signingStyle</key><string>manual</string>
  <key>signingCertificate</key><string>$ASC_SIGN_IDENTITY</string>
  <key>provisioningProfiles</key><dict>
    <key>com.nimmesh.app</key><string>$ADHOC_PROFILE_NAME</string>
  </dict>
  <key>destination</key><string>export</string>
  <key>compileBitcode</key><false/>
  <key>thinning</key><string>&lt;none&gt;</string>
</dict></plist>
PLIST
echo "==> exporting ad-hoc .ipa"
xcodebuild -exportArchive \
  -archivePath "$ARCHIVE" \
  -exportPath "$EXPORT_DIR" \
  -exportOptionsPlist "$BUILD/ExportOptions.plist" \
  | grep -E "Export (succeeded|failed)|error:|\*\* EXPORT" || true
IPA="$(/bin/ls "$EXPORT_DIR"/*.ipa 2>/dev/null | head -1 || true)"
[ -n "$IPA" ] && [ -f "$IPA" ] || { echo "ERROR: no .ipa produced (see export errors above)"; exit 1; }
echo "==> built: $IPA ($(du -h "$IPA" | cut -f1)) — version $MARKETING_VERSION build $BUILD_NUMBER"
