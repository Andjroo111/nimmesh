#!/usr/bin/env bash
# build-testflight.sh — archive nimmesh and upload it to TestFlight, fully headless from the
# Mac Mini (no Xcode GUI, no same-network requirement once it's on TestFlight).
#
# ONE-TIME SETUP (after enrolling in the $99 Apple Developer Program):
#   1. App Store Connect -> Users and Access -> Integrations -> App Store Connect API ->
#      generate an API key (Access: "App Manager"). Download AuthKey_XXXXXXXXXX.p8 (one chance).
#   2. Place the key where altool looks for it:
#        mkdir -p ~/.appstoreconnect/private_keys
#        mv ~/Downloads/AuthKey_*.p8 ~/.appstoreconnect/private_keys/
#   3. Create ~/secrets/testflight.env  (chmod 600) with:
#        ASC_TEAM_ID=ABCDE12345        # PAID team id (Apple Developer -> Membership details)
#        ASC_KEY_ID=XXXXXXXXXX         # the API Key ID (must match AuthKey_<KEY_ID>.p8)
#        ASC_ISSUER_ID=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx   # the Issuer ID (above the keys list)
#   4. In App Store Connect, create the app record once: + New App, platform iOS, name "nimmesh",
#      bundle id com.nimmesh.app, SKU nimmesh. (Required before the first upload appears.)
#
# USAGE:
#   apple/scripts/build-testflight.sh              # framework + archive + upload (the normal loop)
#   apple/scripts/build-testflight.sh --skip-rust  # skip the cargo-swift framework rebuild (faster
#                                                  # when only Swift/webui changed)
#   apple/scripts/build-testflight.sh --no-upload  # build the signed .ipa only (dry run)
#
# After upload: it appears in TestFlight after a few minutes of processing. You (an internal
# tester) tap "Update" in the TestFlight app on your phone — from anywhere, no shared network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
APPLE="$REPO/apple"
BUILD="$REPO/build/testflight"
ARCHIVE="$BUILD/NimmeshApp.xcarchive"
EXPORT_DIR="$BUILD/export"

SKIP_RUST=0; DO_UPLOAD=1
for a in "$@"; do
  case "$a" in
    --skip-rust) SKIP_RUST=1 ;;
    --no-upload) DO_UPLOAD=0 ;;
    *) echo "unknown arg: $a (use --skip-rust or --no-upload)"; exit 2 ;;
  esac
done

# --- config (kept out of git, in the secrets dir) ---
CFG="$HOME/secrets/testflight.env"
if [ ! -f "$CFG" ]; then
  echo "ERROR: missing $CFG"
  echo "       create it (chmod 600) with ASC_TEAM_ID / ASC_KEY_ID / ASC_ISSUER_ID — see the"
  echo "       'ONE-TIME SETUP' header in $(basename "$0"). Also enroll in the \$99 program first."
  exit 1
fi
set -a; . "$CFG"; set +a
: "${ASC_TEAM_ID:?set ASC_TEAM_ID in $CFG}"
: "${ASC_KEY_ID:?set ASC_KEY_ID in $CFG}"
: "${ASC_ISSUER_ID:?set ASC_ISSUER_ID in $CFG}"
KEYFILE="$HOME/.appstoreconnect/private_keys/AuthKey_${ASC_KEY_ID}.p8"
[ -f "$KEYFILE" ] || { echo "ERROR: API key not found at $KEYFILE"; exit 1; }

source "$HOME/.cargo/env"

# --- versioning: marketing version from the cargo workspace; build number = timestamp (unique) ---
MARKETING_VERSION="$(grep -m1 '^version' "$REPO/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
BUILD_NUMBER="$(date +%Y%m%d%H%M)"
echo "==> nimmesh $MARKETING_VERSION (build $BUILD_NUMBER) -> TestFlight (team $ASC_TEAM_ID)"

# --- 1. (re)generate the Rust framework + the Xcode project ---
if [ "$SKIP_RUST" -eq 0 ]; then
  echo "==> cargo swift package (iOS framework)"
  ( cd "$REPO/crates/nimmesh-core" && cargo swift package -n NimmeshCore -p ios -y >/dev/null )
fi
echo "==> xcodegen generate"
( cd "$APPLE" && xcodegen generate >/dev/null )

# --- 2. archive (Release, device, App Store distribution signing via automatic + the paid team) ---
rm -rf "$BUILD"; mkdir -p "$BUILD"
echo "==> archiving (this is the slow step)"
xcodebuild archive \
  -project "$APPLE/NimmeshApp.xcodeproj" \
  -scheme NimmeshApp \
  -configuration Release \
  -sdk iphoneos \
  -destination 'generic/platform=iOS' \
  -archivePath "$ARCHIVE" \
  -allowProvisioningUpdates \
  DEVELOPMENT_TEAM="$ASC_TEAM_ID" \
  CODE_SIGN_STYLE=Automatic \
  MARKETING_VERSION="$MARKETING_VERSION" \
  CURRENT_PROJECT_VERSION="$BUILD_NUMBER" \
  | grep -E "Archive (succeeded|failed)|error:|\*\* ARCHIVE" || true
[ -d "$ARCHIVE" ] || { echo "ERROR: archive not produced"; exit 1; }

# --- 3. export the App Store .ipa ---
cat > "$BUILD/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>app-store-connect</string>
  <key>teamID</key><string>$ASC_TEAM_ID</string>
  <key>signingStyle</key><string>automatic</string>
  <key>destination</key><string>export</string>
  <key>uploadSymbols</key><true/>
  <key>manageAppVersionAndBuildNumber</key><false/>
</dict></plist>
PLIST
echo "==> exporting .ipa"
xcodebuild -exportArchive \
  -archivePath "$ARCHIVE" \
  -exportPath "$EXPORT_DIR" \
  -exportOptionsPlist "$BUILD/ExportOptions.plist" \
  -allowProvisioningUpdates \
  | grep -E "Export (succeeded|failed)|error:|\*\* EXPORT" || true
IPA="$(/bin/ls "$EXPORT_DIR"/*.ipa 2>/dev/null | head -1 || true)"
[ -n "$IPA" ] && [ -f "$IPA" ] || { echo "ERROR: no .ipa produced (see export errors above)"; exit 1; }
echo "==> built: $IPA"

# --- 4. validate + upload to TestFlight (headless via the API key) ---
if [ "$DO_UPLOAD" -eq 1 ]; then
  echo "==> validating"
  xcrun altool --validate-app -f "$IPA" -t ios --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID" \
    || { echo "ERROR: validation failed (see above)"; exit 1; }
  echo "==> uploading to App Store Connect"
  xcrun altool --upload-app -f "$IPA" -t ios --apiKey "$ASC_KEY_ID" --apiIssuer "$ASC_ISSUER_ID"
  echo ""
  echo "✅ uploaded build $BUILD_NUMBER ($MARKETING_VERSION)."
  echo "   It processes for a few minutes, then shows up in App Store Connect -> TestFlight."
  echo "   First time: add yourself as an Internal Tester + install the TestFlight app on your phone."
  echo "   After that: tap Update in TestFlight to get each new build, from anywhere."
else
  echo "==> --no-upload set; signed .ipa is at $IPA"
fi
