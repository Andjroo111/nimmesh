#!/usr/bin/env bash
# build-apk.sh: build the installable NIMmesh APK, headlessly, on the Mac Mini.
#
# This is the Android twin of apple/scripts/build-adhoc.sh, and the reason the whole epic
# exists: the output installs on ANY Android phone from ANY browser. No device registration,
# no 100-device cap, no store review, no Safari-only step.
#
# USAGE:
#   android/scripts/build-apk.sh              # release, all ABIs
#   android/scripts/build-apk.sh --debug      # debug build, faster, debug-signed
#   android/scripts/build-apk.sh --skip-rust  # reuse the existing core + bindings
#   android/scripts/build-apk.sh --verify     # then install on a connected device and PROVE it runs
#
# Output: android/app/build/outputs/apk/release/app-release.apk
#         copied to ota/nimmesh.apk when it is signed.
#
# ── SIGNING ───────────────────────────────────────────────────────────────────────────────
# Release signing is OWNER-GATED (ADR-0002). The keystore never enters the repo. Without it
# this produces an UNSIGNED release APK, which Android will refuse to install: that is the
# correct outcome, not a failure to work around.
#
# One-time keystore creation, on the machine that will keep it:
#
#   keytool -genkeypair -v \
#     -keystore ~/secrets/nimmesh-release.jks \
#     -alias nimmesh -keyalg RSA -keysize 4096 -validity 10000
#
# ⚠ BACK IT UP AND NEVER LOSE IT. Android identifies an app by its SIGNING KEY. Lose the
# keystore and no future build can ever update an existing install; every user has to
# uninstall (destroying their wallet if they have not written down their words) and start
# again. There is no recovery, no appeal, and no Apple-style revocation to fall back on.
#
# Then export, e.g. from ~/secrets/all-keys.env:
#   NIMMESH_KEYSTORE=~/secrets/nimmesh-release.jks
#   NIMMESH_KEYSTORE_PASSWORD=...
#   NIMMESH_KEY_ALIAS=nimmesh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
. "$SCRIPT_DIR/android-env.sh"

BUILD_TYPE=release
SKIP_RUST=0
VERIFY=0
for a in "$@"; do
  case "$a" in
    --debug) BUILD_TYPE=debug ;;
    --skip-rust) SKIP_RUST=1 ;;
    --verify) VERIFY=1 ;;
    *) echo "unknown arg: $a (use --debug | --skip-rust | --verify)"; exit 2 ;;
  esac
done

# The marketing version is the Rust core's, so the APK and the core it embeds can never
# disagree about what they are. Same rule as build-adhoc.sh.
VERSION_NAME="$(grep -m1 '^version' "$REPO/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"

# ⚠ versionCode is an INT and Android caps it at 2147483647, so the iOS trick of stamping
# YYYYMMDDHHMM (12 digits) overflows. Minutes since 2020-01-01 UTC increments once a minute,
# is monotonic, and does not reach the cap for another four thousand years.
EPOCH_2020=1577836800
VERSION_CODE=$(( ( $(date -u +%s) - EPOCH_2020 ) / 60 ))

echo "==> NIMmesh $VERSION_NAME (versionCode $VERSION_CODE) -> $BUILD_TYPE apk"

if [ "$SKIP_RUST" -eq 0 ]; then
  echo "==> building the Rust core + Kotlin bindings"
  # ⚠ Filter this: cargo-ndk dumps the ENTIRE environment into its panic report, and this
  # shell has every key from all-keys.env in it (~/.zshrc loads them). A failing run must
  # never paste secrets into a log.
  if ! "$SCRIPT_DIR/build-core.sh" > /tmp/nimmesh-build-core.log 2>&1; then
    echo "build-core.sh FAILED. Reason (environment dump filtered out):"
    grep -vE '^\s+[A-Za-z_]+=' /tmp/nimmesh-build-core.log | tail -20
    exit 1
  fi
fi

cd "$REPO/android"
if [ "$BUILD_TYPE" = "release" ]; then
  ./gradlew --no-daemon :app:assembleRelease \
    -PnimmeshVersionName="$VERSION_NAME" -PnimmeshVersionCode="$VERSION_CODE"
  APK="$REPO/android/app/build/outputs/apk/release/app-release.apk"
  UNSIGNED="$REPO/android/app/build/outputs/apk/release/app-release-unsigned.apk"
  [ -f "$APK" ] || APK="$UNSIGNED"
else
  ./gradlew --no-daemon :app:assembleDebug \
    -PnimmeshVersionName="$VERSION_NAME" -PnimmeshVersionCode="$VERSION_CODE"
  APK="$REPO/android/app/build/outputs/apk/debug/app-debug.apk"
fi

[ -f "$APK" ] || { echo "no APK at $APK"; exit 1; }

APKSIGNER="$ANDROID_HOME/build-tools/36.0.0/apksigner"
echo
echo "==> $(basename "$APK"), $(wc -c < "$APK" | tr -d ' ') bytes"

if "$APKSIGNER" verify --print-certs "$APK" >/tmp/nimmesh-apk-certs.txt 2>&1; then
  echo "==> signed:"
  grep -E 'Signer #1 certificate (DN|SHA-256)' /tmp/nimmesh-apk-certs.txt | sed 's/^/    /'
  if [ "$BUILD_TYPE" = "release" ]; then
    cp "$APK" "$REPO/ota/nimmesh.apk"
    echo "==> copied to ota/nimmesh.apk for the install page"
  fi
else
  echo "==> UNSIGNED. Android will refuse to install this."
  echo "    Set NIMMESH_KEYSTORE and NIMMESH_KEYSTORE_PASSWORD (see the header of this script)."
  echo "    Not copied to ota/ : publishing an uninstallable APK is worse than publishing none."
fi

if [ "$VERIFY" -eq 1 ]; then
  # ── Why this exists ────────────────────────────────────────────────────────────────────
  # The release build runs R8, and a mangled FFI surface fails at RUNTIME on a user's phone,
  # not at build time. The instrumented tests are built against DEBUG, so they cannot catch
  # it. The only honest check is to run the shipping artifact and watch the Rust core accept
  # a signature made by Kotlin.
  echo
  echo "==> verifying the built APK on a connected device"
  command -v adb >/dev/null || { echo "adb not on PATH"; exit 1; }
  adb get-state >/dev/null 2>&1 || { echo "no device or emulator attached"; exit 1; }

  VERIFY_APK="$APK"
  if ! "$APKSIGNER" verify "$APK" >/dev/null 2>&1; then
    # Sign a COPY with the debug key purely to make it installable. This proves the code
    # survived R8; it says nothing about release signing, which is owner-gated.
    echo "    (unsigned: signing a throwaway copy with the debug key so it can be installed)"
    VERIFY_APK=/tmp/nimmesh-verify.apk
    cp "$APK" "$VERIFY_APK"
    "$APKSIGNER" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android \
      --key-pass pass:android --ks-key-alias androiddebugkey "$VERIFY_APK"
  fi

  adb uninstall com.nimmesh.app >/dev/null 2>&1 || true
  adb install -r "$VERIFY_APK" >/dev/null
  for p in BLUETOOTH_SCAN BLUETOOTH_ADVERTISE BLUETOOTH_CONNECT POST_NOTIFICATIONS; do
    adb shell pm grant com.nimmesh.app "android.permission.$p" >/dev/null 2>&1 || true
  done

  adb logcat -c
  adb shell am start -n com.nimmesh.app/.MainActivity >/dev/null
  sleep 8
  # Onboarding first: no wallet means the self-test has nothing to sign with, so create one.
  adb shell input tap 540 1804 >/dev/null 2>&1
  sleep 5
  adb shell am force-stop com.nimmesh.app >/dev/null
  adb logcat -c
  adb shell am start -n com.nimmesh.app/.MainActivity >/dev/null
  sleep 9

  SELF_TEST="$(adb logcat -d -s nimmesh.app 2>/dev/null | grep -o 'wallet self-test:.*' | tail -1)"
  echo "    $SELF_TEST"
  case "$SELF_TEST" in
    *"signedOk=true"*)
      echo "    OK: the Rust core accepted a signature made in Kotlin, through the shipped APK." ;;
    *)
      echo "    FAILED: the FFI did not survive this build. Check app/proguard-rules.pro."
      exit 1 ;;
  esac
  # Uninstall, not just pm clear. Two reasons: never leave a wallet behind on a test device,
  # and a release build's versionCode is minutes-since-2020 while a plain `./gradlew
  # assembleDebug` falls back to 1, so leaving the release installed makes every later debug
  # install fail with INSTALL_FAILED_VERSION_DOWNGRADE.
  adb uninstall com.nimmesh.app >/dev/null 2>&1 || true
fi
