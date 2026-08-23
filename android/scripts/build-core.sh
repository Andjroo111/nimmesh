#!/usr/bin/env bash
# build-core.sh: cross-compile nimmesh-core for Android and generate its Kotlin bindings.
#
# Produces, into android/core/src/main/ (both GITIGNORED, both regenerated from source):
#   jniLibs/<abi>/libnimmesh_core.so   the Rust core, one per ABI
#   kotlin/uniffi/nimmesh_core/*.kt    the UniFFI bindings
#
# The bindings are generated FROM THE BUILT LIBRARY (`--library`), not from a UDL, so
# the binding's contract version can never drift from the runtime's. That drift is the
# exact failure the iOS side is pinned against (see the workspace Cargo.toml comment on
# `uniffi = "=0.31.1"`): a mismatch aborts the app at launch. Generating off the .so
# means the two are the same artifact by construction.
#
# Features match apple/scripts/build-adhoc.sh exactly, so the Kotlin FFI surface is the
# same shape as the Swift one. Anything the bridge does not wire up simply goes unused.
#
# USAGE:
#   android/scripts/build-core.sh              # release, all ABIs
#   android/scripts/build-core.sh --debug      # debug profile (faster, much larger .so)
#   android/scripts/build-core.sh --abi arm64-v8a
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
. "$SCRIPT_DIR/android-env.sh"

# minSdk 31. Chosen so BLUETOOTH_SCAN can carry `neverForLocation` and the app never has
# to ask for location at all; below API 31 there is no way to scan without ACCESS_FINE_LOCATION.
PLATFORM=31
PROFILE=release
ABIS="arm64-v8a armeabi-v7a x86_64"   # x86 is dead at minSdk 31; x86_64 is here for the emulator

while [ $# -gt 0 ]; do
  case "$1" in
    --debug) PROFILE=debug; shift ;;
    --abi) ABIS="$2"; shift 2 ;;
    *) echo "unknown arg: $1 (use --debug | --abi <abi>)"; exit 2 ;;
  esac
done

OUT_JNI="$REPO/android/core/src/main/jniLibs"
OUT_KT="$REPO/android/core/src/main/kotlin"
FEATURES="gateway-rpc,polygon-gateway"

TARGET_ARGS=""
for abi in $ABIS; do TARGET_ARGS="$TARGET_ARGS -t $abi"; done

echo "==> cargo ndk build ($PROFILE) for:$TARGET_ARGS"
rm -rf "$OUT_JNI"; mkdir -p "$OUT_JNI"
cd "$REPO/crates/nimmesh-core"
# shellcheck disable=SC2086
if [ "$PROFILE" = "release" ]; then
  cargo ndk $TARGET_ARGS --platform "$PLATFORM" -o "$OUT_JNI" build --release --features "$FEATURES"
else
  cargo ndk $TARGET_ARGS --platform "$PLATFORM" -o "$OUT_JNI" build --features "$FEATURES"
fi

# Any ABI's library carries the same UniFFI metadata; use the first one built.
FIRST_ABI="$(echo "$ABIS" | awk '{print $1}')"
LIB="$OUT_JNI/$FIRST_ABI/libnimmesh_core.so"
[ -f "$LIB" ] || { echo "build-core: expected $LIB"; exit 1; }

echo "==> uniffi-bindgen generate --language kotlin (from $FIRST_ABI/libnimmesh_core.so)"
rm -rf "$OUT_KT"; mkdir -p "$OUT_KT"
cd "$REPO"
# --no-format: ktlint is not installed on this box and the generator only warns, but the
# warning reads like an error in a build log. The output is already well formed.
cargo run --quiet -p nimmesh-core --bin uniffi-bindgen --features "$FEATURES" -- generate \
  --library "$LIB" --language kotlin --no-format --out-dir "$OUT_KT"

echo
echo "==> built:"
for abi in $ABIS; do
  f="$OUT_JNI/$abi/libnimmesh_core.so"
  [ -f "$f" ] && printf '    %-14s %s bytes\n' "$abi" "$(wc -c < "$f" | tr -d ' ')"
done
while IFS= read -r kt; do
  printf '    bindings       %s lines (%s)\n' "$(wc -l < "$kt" | tr -d ' ')" "${kt#"$REPO/"}"
done < <(find "$OUT_KT" -name '*.kt')
